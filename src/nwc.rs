//! NWC-backed `WalletProvider`. This is the only module that depends on NIP-47
//! client types.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use nwc::prelude::{
    ErrorCode, ListTransactionsRequest, LookupInvoiceRequest, LookupInvoiceResponse,
    MakeInvoiceRequest, NostrWalletConnect, NostrWalletConnectUri, PayInvoiceRequest,
    TransactionState, TransactionType,
};

use mesh_llm_wallet::contract::WalletIdentity;
use mesh_llm_wallet::invoice::Invoice;
use mesh_llm_wallet::provider::{Balance, PayError, PaymentStatus, Transaction, WalletProvider};

/// File in the host-supplied wallet directory holding the connection secret.
pub const URI_FILE: &str = "nwc-uri.txt";
/// Fallback when no file exists: the `url` field of the `[[plugin]]` stanza.
const URI_ENV: &str = "MESH_LLM_PLUGIN_URL";

/// Wallets that pay synchronously (Alby Hub) answer `pay_invoice` only once the
/// payment resolves, so give routing time before calling the outcome uncertain.
const PAY_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_POLL_BACKOFF: Duration = Duration::from_secs(10);

pub struct NwcProvider {
    client: NostrWalletConnect,
}

/// Result of opening: the provider plus the identity the host pins.
pub struct Opened {
    pub provider: NwcProvider,
    pub identity: WalletIdentity,
}

impl NwcProvider {
    pub async fn open(directory: &Path) -> Result<Opened> {
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let uri = read_uri(directory)?;
        let wallet_pubkey = uri.public_key.to_hex();
        let client = NostrWalletConnect::new(uri);
        let info = client
            .get_info()
            .await
            .map_err(|error| anyhow::anyhow!("NWC get_info failed: {error}"))?;
        let network = normalize_network(info.network.as_deref());
        // The wallet service key identifies this connection's wallet. A Hub
        // sub-wallet shares the node key but not its balance, so the node key
        // would be the wrong pin.
        let identity = WalletIdentity {
            wallet_id: wallet_pubkey,
            provider: "nwc".into(),
            network,
        };
        Ok(Opened {
            provider: Self { client },
            identity,
        })
    }

    fn transaction(tx: LookupInvoiceResponse) -> Transaction {
        let inbound = tx.transaction_type == Some(TransactionType::Incoming);
        let state = tx.state.unwrap_or_else(|| infer_state(&tx));
        let status = match state {
            TransactionState::Settled => PaymentStatus::Succeeded,
            TransactionState::Failed | TransactionState::Expired => PaymentStatus::Failed,
            TransactionState::Pending | TransactionState::Accepted => PaymentStatus::Pending,
        };
        Transaction {
            id: tx.payment_hash.clone(),
            payment_hash: Some(tx.payment_hash),
            inbound,
            amount_msat: tx.amount,
            fee_msat: tx.fees_paid,
            status,
            // `accepted` is NIP-47's held-HTLC state: committed, not yet settled.
            claiming: inbound && state == TransactionState::Accepted,
            status_msg: Some(format!("{state:?}").to_lowercase()),
            created_at_ms: tx.created_at.as_secs() * 1000,
            settled_at_ms: tx.settled_at.map(|time| time.as_secs() * 1000),
        }
    }

    async fn wait_until(
        &self,
        payment_hash: &str,
        done: impl Fn(&Transaction) -> bool,
    ) -> Result<Transaction> {
        // NIP-47 notifications are optional and not every wallet sends them,
        // so poll. Lookup errors back off and retry: the wait outlasts a relay
        // or wallet outage, and callers that need a deadline impose their own.
        let mut delay = POLL_INTERVAL;
        loop {
            match self.lookup(payment_hash).await {
                Ok(Some(payment)) if done(&payment) => return Ok(payment),
                Ok(_) => delay = POLL_INTERVAL,
                Err(error) => {
                    tracing::debug!(%error, "NWC payment lookup failed; retrying");
                    delay = (delay * 2).min(MAX_POLL_BACKOFF);
                }
            }
            tokio::time::sleep(delay).await;
        }
    }
}

fn read_uri(directory: &Path) -> Result<NostrWalletConnectUri> {
    let path = directory.join(URI_FILE);
    let raw = match OpenOptions::new().read(true).open(&path) {
        Ok(mut file) => {
            restrict_permissions(&path)?;
            let mut raw = String::new();
            file.read_to_string(&mut raw)
                .context("could not read NWC connection secret")?;
            raw
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::env::var(URI_ENV)
            .map_err(|_| {
                anyhow::anyhow!(
                    "no NWC connection secret: write it to {} or set the plugin `url`",
                    path.display()
                )
            })?,
        Err(error) => return Err(anyhow::Error::new(error).context("open NWC connection secret")),
    };
    // Never echo the URI: it carries the connection secret.
    NostrWalletConnectUri::parse(raw.trim())
        .map_err(|_| anyhow::anyhow!("invalid NWC connection secret"))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).context("NWC connection secret is unreadable")?;
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("could not restrict NWC connection secret permissions")?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// The host pins and checks `mainnet`; wallets report `bitcoin` or `mainnet`.
fn normalize_network(network: Option<&str>) -> String {
    match network.map(str::to_ascii_lowercase).as_deref() {
        None | Some("bitcoin" | "mainnet") => "mainnet".into(),
        Some(other) => other.into(),
    }
}

/// Wallets predating the NIP-47 `state` field only report timestamps.
fn infer_state(tx: &LookupInvoiceResponse) -> TransactionState {
    if tx.settled_at.is_some() {
        return TransactionState::Settled;
    }
    let now = mesh_llm_wallet::now_ms() / 1000;
    match (tx.transaction_type, tx.expires_at) {
        (Some(TransactionType::Incoming), Some(expires)) if expires.as_secs() < now => {
            TransactionState::Expired
        }
        _ => TransactionState::Pending,
    }
}

/// The NIP-47 error code a wallet returned, if any. The client flattens wallet
/// errors into `"<message> [<Code>]"`; this is the one place that string is read.
fn error_code(error: &nwc::prelude::Error) -> Option<ErrorCode> {
    const CODES: [(&str, ErrorCode); 10] = [
        ("[RateLimited]", ErrorCode::RateLimited),
        ("[NotImplemented]", ErrorCode::NotImplemented),
        ("[InsufficientBalance]", ErrorCode::InsufficientBalance),
        ("[PaymentFailed]", ErrorCode::PaymentFailed),
        ("[NotFound]", ErrorCode::NotFound),
        ("[QuotaExceeded]", ErrorCode::QuotaExceeded),
        ("[Restricted]", ErrorCode::Restricted),
        ("[Unauthorized]", ErrorCode::Unauthorized),
        ("[Internal]", ErrorCode::Internal),
        ("[Other]", ErrorCode::Other),
    ];
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(current) = source {
        let text = current.to_string();
        if let Some((_, code)) = CODES.iter().find(|(tag, _)| text.trim_end().ends_with(tag)) {
            return Some(*code);
        }
        source = current.source();
    }
    None
}

/// Codes a wallet returns after refusing a request, before any payment attempt.
fn rejected_before_submission(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::RateLimited
            | ErrorCode::NotImplemented
            | ErrorCode::InsufficientBalance
            | ErrorCode::QuotaExceeded
            | ErrorCode::Restricted
            | ErrorCode::Unauthorized
    )
}

#[async_trait]
impl WalletProvider for NwcProvider {
    async fn balance(&self) -> Result<Balance> {
        let balance = self
            .client
            .get_balance()
            .await
            .map_err(|error| anyhow::anyhow!("NWC balance query failed: {error}"))?;
        Ok(Balance {
            spendable_msat: balance.balance,
        })
    }

    async fn transactions(&self, limit: usize) -> Result<Vec<Transaction>> {
        ensure!(
            (1..=1000).contains(&limit),
            "transaction limit must be between 1 and 1000"
        );
        let transactions = self
            .client
            .list_transactions(ListTransactionsRequest {
                limit: Some(limit as u64),
                ..Default::default()
            })
            .await
            .map_err(|error| anyhow::anyhow!("NWC transaction query failed: {error}"))?;
        Ok(transactions.into_iter().map(Self::transaction).collect())
    }

    async fn create_invoice(&self, amount_msat: Option<u64>, expiry_secs: u32) -> Result<Invoice> {
        let Some(amount_msat) = amount_msat else {
            bail!("NWC make_invoice requires an amount");
        };
        ensure!(amount_msat > 0, "zero amount invoice is not supported");
        ensure!(expiry_secs > 0, "invoice expiry must be positive");
        let requested_expiry_ms = mesh_llm_wallet::now_ms() + u64::from(expiry_secs) * 1000;
        let response = self
            .client
            .make_invoice(MakeInvoiceRequest {
                amount: amount_msat,
                description: Some("mesh-llm".into()),
                description_hash: None,
                // Always explicit: the wallet default is typically a day, and
                // the host relies on short inference invoices expiring.
                expiry: Some(u64::from(expiry_secs)),
            })
            .await
            .map_err(|error| anyhow::anyhow!("NWC invoice creation failed: {error}"))?;
        let invoice = Invoice::parse(&response.invoice)?;
        // A wallet that ignores `expiry` must not hand the host a longer-lived
        // invoice than it asked for.
        ensure!(
            invoice.expires_at_ms <= requested_expiry_ms + 60_000,
            "wallet ignored the requested invoice expiry"
        );
        Ok(invoice)
    }

    async fn pay(
        &self,
        invoice: &Invoice,
        amount_msat: u64,
        max_total_msat: u64,
    ) -> Result<Transaction, PayError> {
        invoice
            .validate_payment(amount_msat, mesh_llm_wallet::now_ms())
            .map_err(PayError::NotSubmitted)?;
        if amount_msat > max_total_msat {
            return Err(PayError::NotSubmitted(anyhow::anyhow!(
                "payment amount exceeds authorized amount"
            )));
        }
        // A repeat `pay` must observe the earlier attempt, never send again.
        match self.lookup(&invoice.payment_hash).await {
            Ok(Some(existing)) if existing.inbound => {
                return Err(PayError::NotSubmitted(anyhow::anyhow!(
                    "cannot pay this wallet's own invoice"
                )));
            }
            Ok(Some(existing)) => return Ok(existing),
            Ok(None) => {}
            Err(error) => return Err(PayError::NotSubmitted(error)),
        }
        // NIP-47 has no fee limit, so `max_total_msat` cannot be enforced
        // before sending; the wallet applies its own cap (Alby Hub:
        // max(1%, 10 sat)). Overruns are logged below. See README.
        let response = self
            .client
            .pay_invoice(PayInvoiceRequest {
                id: None,
                invoice: invoice.bolt11.clone(),
                amount: invoice.amount_msat.is_none().then_some(amount_msat),
            })
            .timeout(PAY_TIMEOUT)
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if error_code(&error).is_some_and(rejected_before_submission) {
                    return Err(PayError::NotSubmitted(anyhow::anyhow!(
                        "NWC wallet refused payment: {error}"
                    )));
                }
                // A definitive failure is recorded by the wallet; otherwise the
                // outcome stays uncertain for the host to reconcile by hash.
                if let Ok(Some(payment)) = self.lookup(&invoice.payment_hash).await
                    && payment.status == PaymentStatus::Failed
                {
                    return Ok(payment);
                }
                return Err(PayError::Uncertain(anyhow::anyhow!(
                    "NWC payment outcome uncertain; reconcile by payment hash: {error}"
                )));
            }
        };
        let preimage: [u8; 32] = hex_preimage(&response.preimage)
            .ok_or_else(|| PayError::Uncertain(anyhow::anyhow!("wallet returned no preimage")))?;
        if !invoice.verifies_preimage(&preimage) {
            return Err(PayError::Uncertain(anyhow::anyhow!(
                "wallet returned a preimage that does not match the invoice"
            )));
        }
        let fee_msat = response.fees_paid.unwrap_or(0);
        if amount_msat.saturating_add(fee_msat) > max_total_msat {
            tracing::warn!(
                amount_msat,
                fee_msat,
                max_total_msat,
                "NWC payment exceeded the authorized total; NIP-47 cannot cap fees"
            );
        }
        let now_ms = mesh_llm_wallet::now_ms();
        Ok(Transaction {
            id: invoice.payment_hash.clone(),
            payment_hash: Some(invoice.payment_hash.clone()),
            inbound: false,
            amount_msat,
            fee_msat,
            status: PaymentStatus::Succeeded,
            claiming: false,
            status_msg: Some("settled".into()),
            created_at_ms: now_ms,
            settled_at_ms: Some(now_ms),
        })
    }

    async fn lookup(&self, payment_hash: &str) -> Result<Option<Transaction>> {
        let response = self
            .client
            .lookup_invoice(LookupInvoiceRequest {
                payment_hash: Some(payment_hash.into()),
                invoice: None,
            })
            .await;
        match response {
            Ok(tx) => Ok(Some(Self::transaction(tx))),
            Err(error) if error_code(&error) == Some(ErrorCode::NotFound) => Ok(None),
            Err(error) => Err(anyhow::anyhow!("NWC payment status query failed: {error}")),
        }
    }

    async fn wait_for_payment(&self, payment_hash: &str) -> Result<Transaction> {
        self.wait_until(payment_hash, |payment| {
            payment.status != PaymentStatus::Pending
        })
        .await
    }

    async fn wait_for_arrival(&self, payment_hash: &str) -> Result<Transaction> {
        self.wait_until(payment_hash, |payment| {
            payment.status != PaymentStatus::Pending || payment.is_claiming()
        })
        .await
    }
}

fn hex_preimage(preimage: &str) -> Option<[u8; 32]> {
    let preimage = preimage.trim();
    if preimage.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(preimage.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_normalize_to_the_host_vocabulary() {
        assert_eq!(normalize_network(Some("bitcoin")), "mainnet");
        assert_eq!(normalize_network(Some("Mainnet")), "mainnet");
        assert_eq!(normalize_network(None), "mainnet");
        assert_eq!(normalize_network(Some("signet")), "signet");
    }

    #[test]
    fn preimages_parse_only_as_32_byte_hex() {
        assert_eq!(hex_preimage(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(hex_preimage("abcd"), None);
        assert_eq!(hex_preimage(&"zz".repeat(32)), None);
    }

    #[test]
    fn only_refusals_count_as_not_submitted() {
        assert!(rejected_before_submission(ErrorCode::InsufficientBalance));
        assert!(rejected_before_submission(ErrorCode::QuotaExceeded));
        assert!(!rejected_before_submission(ErrorCode::PaymentFailed));
        assert!(!rejected_before_submission(ErrorCode::Internal));
        assert!(!rejected_before_submission(ErrorCode::Other));
    }

    #[cfg(unix)]
    #[test]
    fn secret_permissions_are_tightened_and_secret_is_not_echoed() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(URI_FILE);
        std::fs::write(&path, "nostr+walletconnect://not-a-key?secret=hunter2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_uri(directory.path()).err().unwrap().to_string();
        assert!(!error.contains("hunter2"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn transactions_map_nip47_states() {
        let tx: LookupInvoiceResponse = serde_json::from_value(serde_json::json!({
            "type": "incoming",
            "state": "accepted",
            "payment_hash": "00",
            "amount": 1000,
            "fees_paid": 0,
            "created_at": 1,
        }))
        .unwrap();
        let mapped = NwcProvider::transaction(tx);
        assert_eq!(mapped.status, PaymentStatus::Pending);
        assert!(mapped.is_claiming());
        assert_eq!(mapped.created_at_ms, 1000);
    }
}
