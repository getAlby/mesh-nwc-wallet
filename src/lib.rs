//! External Nostr Wallet Connect (NIP-47) wallet.v1 plugin and reusable backend
//! entrypoint. The host owns payment policy; this library owns NWC wallet I/O.
#![forbid(unsafe_code)]

mod nwc;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use mesh_llm_plugin::PluginRuntime;
use mesh_llm_wallet::backend::{OpenedWallet, WalletBackend};
use mesh_llm_wallet::plugin_server::wallet_plugin;

/// Plugin name the host launches this implementation under.
pub const PLUGIN_NAME: &str = "nwc-wallet";
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct NwcBackend;

#[async_trait]
impl WalletBackend for NwcBackend {
    fn provider_name(&self) -> &'static str {
        "nwc"
    }

    async fn open(&self, directory: &Path) -> Result<OpenedWallet> {
        let opened = nwc::NwcProvider::open(directory).await?;
        Ok(OpenedWallet {
            identity: opened.identity,
            provider: Arc::new(opened.provider),
            created: false,
        })
    }
}

/// Serve the `wallet.v1` capability on the host-supplied plugin endpoint until
/// the host closes the connection. `name` is the plugin name the host launched
/// this process under. Logging is owned by the launching binary.
pub async fn run_plugin(name: String) -> Result<()> {
    PluginRuntime::run(wallet_plugin(name, VERSION, NwcBackend)).await
}
