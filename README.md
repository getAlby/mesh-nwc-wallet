# NWC wallet plugin for mesh-llm

A [mesh-llm](https://github.com/Mesh-LLM/mesh-llm) wallet plugin that pays and
receives over [Nostr Wallet Connect (NIP-47)](https://github.com/nostr-protocol/nips/blob/master/47.md).
Point it at [Alby Hub](https://albyhub.com) or any other NWC wallet and mesh-llm
can pay for, and get paid for, inference with it.

It implements mesh-llm's `wallet.v1` plugin capability, the same interface as the
[Lexe wallet plugin](https://github.com/Mesh-LLM/lexe-wallet).

> [!WARNING]
> Early proof of concept, like mesh-llm's Lightning payments themselves. Use a
> dedicated NWC connection with a small budget.

## Setup

1. **Create a connection** in your wallet. In Alby Hub: *Connections → Add
   connection*, enable *Send payments*, *Create invoices*, *Lookup status of
   invoices*, *Read your balance*, *Read transaction history* and *Read your
   node info*, and set a budget.

2. **Build the plugin** (Rust 1.88+):

   ```bash
   cargo build --release --locked
   ```

3. **Register it** in mesh-llm's `config.toml`:

   ```toml
   [[plugin]]
   name = "nwc-wallet"
   command = "/path/to/target/release/nwc-wallet"

   [payments]
   wallet = "nwc-wallet"   # optional when it is the only wallet plugin
   ```

4. **Store the connection secret** in the data directory mesh-llm hands the
   plugin, `<mesh-llm profile>/payments/wallets/nwc-wallet/`:

   ```bash
   echo 'nostr+walletconnect://...' > <profile>/payments/wallets/nwc-wallet/nwc-uri.txt
   ```

   The plugin restricts the file to `0600` on every open. As a fallback it reads
   the stanza's `url = "nostr+walletconnect://..."`, which mesh-llm passes as
   `MESH_LLM_PLUGIN_URL`. Avoid that if your `config.toml` is shared or synced.

The plugin connects only when mesh-llm opens the wallet for a payment operation.

## Behavior

| `wallet.v1` | NIP-47 |
|---|---|
| balance | `get_balance` |
| transactions | `list_transactions` |
| create invoice | `make_invoice`, with the host's expiry always sent explicitly |
| pay | `lookup_invoice`, then `pay_invoice` |
| lookup | `lookup_invoice` (`NOT_FOUND` means no record) |
| wait for payment / arrival | polls `lookup_invoice` once a second |

- **Wallet identity**: the wallet service public key from the connection URI.
  Switching to a different connection counts as switching wallets, so run
  `mesh-llm wallet unpin` first.
- **Repeat payments**: `pay` looks up the payment hash first. A repeat call
  returns the earlier attempt and never sends twice.
- **Uncertain outcomes**: payment is reported as *not submitted* only when the
  wallet refuses before trying (`INSUFFICIENT_BALANCE`, `QUOTA_EXCEEDED`,
  `RATE_LIMITED`, `RESTRICTED`, `UNAUTHORIZED`, `NOT_IMPLEMENTED`). Timeouts and
  other errors stay *uncertain* for mesh-llm to reconcile by payment hash,
  unless the wallet already records the payment as failed.
- **Success** requires a preimage that matches the invoice's payment hash.
- **Held HTLCs**: NIP-47's `accepted` state counts as mesh-llm's *claiming*
  signal for incoming payments.
- **Amountless invoices** cannot be created, because NIP-47 `make_invoice`
  requires an amount.

### Fee cap limitation

mesh-llm tells the wallet the most a payment may cost including routing fees
(`max_total_msat`). For inference payments it allows **max(3 sats, 1%)** in
fees. NIP-47 `pay_invoice` has no way to pass a fee limit, so this plugin
**cannot enforce that cap before paying**. The wallet applies its own limit
instead. Alby Hub caps routing fees at **max(10 sats, 1%)**.

In practice, small payments over direct channels cost close to nothing. When a
payment does go over the cap, the plugin logs a warning with the actual fee.
The fix is an optional fee-limit parameter on NIP-47 `pay_invoice`.

## Development

```bash
just check   # fmt, check, clippy -D warnings
just test    # unit + installed-process tests; no network, no funds
```

The live round trip pays **1 real sat** between two NWC wallets through two
plugin processes. It only runs when asked:

```bash
NWC_TEST_PAYER_URI='nostr+walletconnect://...' \
NWC_TEST_PAYEE_URI='nostr+walletconnect://...' \
cargo test --test installed_process live_payment_round_trip -- --ignored --nocapture
```

## License

Apache-2.0
