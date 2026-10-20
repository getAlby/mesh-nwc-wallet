//! Standalone launcher; wallet state is opened only through wallet.v1 RPC.
use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let name = std::env::var("MESH_LLM_PLUGIN_NAME")
        .context("Launch this executable through mesh-llm plugins, not directly")?;
    mesh_wallet_nwc::run_plugin(name).await
}
