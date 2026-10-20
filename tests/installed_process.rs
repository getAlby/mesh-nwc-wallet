//! Real host package installer and protocol. The default test never contacts a
//! wallet; `live_payment_round_trip` does, and only when explicitly enabled.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::time::Duration;

use mesh_llm_plugin::{LocalStream, PROTOCOL_VERSION, proto, read_envelope, write_envelope};
use mesh_llm_plugin_manager::install::install_plugin_archive;
use mesh_llm_plugin_manager::{PluginInstallOptions, PluginTarget};
use proto::envelope::Payload;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::timeout;

fn install(root: &Path) -> PathBuf {
    let archive = root.join("nwc-wallet.tar.gz");
    let compressed = flate2::write::GzEncoder::new(
        std::fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(compressed);
    tar.append_path_with_name(env!("CARGO_BIN_EXE_nwc-wallet"), "nwc-wallet/nwc-wallet")
        .unwrap();
    tar.append_path_with_name("plugin.toml", "nwc-wallet/plugin.toml")
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let options = PluginInstallOptions {
        store_root: root.join("store"),
        install_root: root.join("installed"),
        catalog_url: "http://unused.invalid".into(),
        target: PluginTarget::current().unwrap(),
    };
    let outcome =
        install_plugin_archive("nwc-wallet", "0.1.0", &archive, &options, &mut |_| {}).unwrap();
    assert!(outcome.metadata.enabled);
    root.join("installed/nwc-wallet/nwc-wallet")
}

async fn exchange(stream: &mut LocalStream, id: u64, payload: Payload, wait: Duration) -> Payload {
    write_envelope(
        stream,
        &proto::Envelope {
            protocol_version: PROTOCOL_VERSION,
            plugin_id: "nwc-wallet".into(),
            request_id: id,
            payload: Some(payload),
        },
    )
    .await
    .unwrap();
    let response = timeout(wait, read_envelope(stream)).await.unwrap().unwrap();
    assert_eq!(response.request_id, id);
    response.payload.unwrap()
}

/// Invoke a wallet.v1 operation; `Err` carries the plugin's error JSON.
async fn invoke(
    stream: &mut LocalStream,
    id: u64,
    service: &str,
    input: Value,
    wait: Duration,
) -> Result<Value, Value> {
    let response = exchange(
        stream,
        id,
        Payload::InvokeServiceRequest(proto::InvokeServiceRequest {
            kind: 1,
            service_name: service.into(),
            input_json: input.to_string(),
        }),
        wait,
    )
    .await;
    let Payload::InvokeServiceResponse(response) = response else {
        panic!("RPC failed")
    };
    let output: Value = serde_json::from_str(&response.output_json).unwrap();
    if response.is_error {
        Err(output)
    } else {
        Ok(output)
    }
}

async fn launch(binary: &Path, root: &Path, socket: &str) -> (Child, LocalStream) {
    let socket = root.join(socket);
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let child = Command::new(binary)
        .env("HOME", root)
        .env("MESH_LLM_PLUGIN_NAME", "nwc-wallet")
        .env("MESH_LLM_PLUGIN_ENDPOINT", &socket)
        .env("MESH_LLM_PLUGIN_TRANSPORT", "unix")
        .env_remove("MESH_LLM_PLUGIN_URL")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    (child, LocalStream::Unix(stream))
}

async fn initialize(stream: &mut LocalStream) {
    let response = exchange(
        stream,
        1,
        Payload::InitializeRequest(proto::InitializeRequest {
            host_protocol_version: PROTOCOL_VERSION,
            host_version: "test".into(),
            host_info_json: "{}".into(),
            mesh_visibility: proto::MeshVisibility::Private.into(),
        }),
        Duration::from_secs(10),
    )
    .await;
    let Payload::InitializeResponse(response) = response else {
        panic!("initialize failed")
    };
    assert_eq!(response.plugin_id, "nwc-wallet");
    assert!(response.capabilities.iter().any(|s| s == "wallet.v1"));
    let manifest = response.manifest.unwrap();
    assert!(
        manifest.operations.is_empty(),
        "wallet must not advertise MCP tools"
    );
    assert!(manifest.http_bindings.is_empty());
}

#[tokio::test]
async fn installed_process_handshakes_and_refuses_open_without_secret() {
    // Short socket path: Darwin limits sockaddr_un to 104 bytes.
    let root = tempfile::Builder::new()
        .prefix("nwc-")
        .tempdir_in("/tmp")
        .unwrap();
    let binary = install(root.path());
    let (mut child, mut stream) = launch(&binary, root.path(), "host.sock").await;
    initialize(&mut stream).await;
    let ten = Duration::from_secs(10);
    let error = invoke(&mut stream, 2, "wallet_balance", json!({}), ten)
        .await
        .unwrap_err();
    assert_eq!(error["kind"], "not_open");
    let directory = root.path().join("payments/wallets/nwc-wallet");
    let error = invoke(
        &mut stream,
        3,
        "wallet_open",
        json!({ "directory": directory }),
        ten,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("nwc-uri.txt"), "{error}");
    let response = exchange(
        &mut stream,
        4,
        Payload::ShutdownRequest(proto::ShutdownRequest {
            reason: "test complete".into(),
        }),
        ten,
    )
    .await;
    assert!(matches!(response, Payload::ShutdownResponse(_)));
    assert!(timeout(ten, child.wait()).await.unwrap().unwrap().success());
}

/// Pays 1 sat from `NWC_TEST_PAYER_URI` to `NWC_TEST_PAYEE_URI` through two
/// plugin processes. Moves real funds on whatever wallets those URIs name, so
/// it runs only with `cargo test -- --ignored` and both variables set.
#[tokio::test]
#[ignore = "contacts live NWC wallets and moves 1 sat"]
async fn live_payment_round_trip() {
    let (Ok(payer_uri), Ok(payee_uri)) = (
        std::env::var("NWC_TEST_PAYER_URI"),
        std::env::var("NWC_TEST_PAYEE_URI"),
    ) else {
        panic!("set NWC_TEST_PAYER_URI and NWC_TEST_PAYEE_URI");
    };
    let root = tempfile::Builder::new()
        .prefix("nwc-")
        .tempdir_in("/tmp")
        .unwrap();
    let binary = install(root.path());
    let wait = Duration::from_secs(120);

    let mut wallets = Vec::new();
    for (name, uri) in [("payer", payer_uri), ("payee", payee_uri)] {
        let directory = root.path().join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("nwc-uri.txt"), uri).unwrap();
        let (child, mut stream) = launch(&binary, root.path(), &format!("{name}.sock")).await;
        initialize(&mut stream).await;
        let opened = invoke(
            &mut stream,
            2,
            "wallet_open",
            json!({ "directory": directory }),
            wait,
        )
        .await
        .unwrap();
        println!("{name} opened: {opened}");
        let balance = invoke(&mut stream, 3, "wallet_balance", json!({}), wait)
            .await
            .unwrap();
        println!("{name} balance: {balance}");
        wallets.push((child, stream));
    }
    let [(_payer_child, payer), (_payee_child, payee)] = &mut wallets[..] else {
        unreachable!()
    };

    let invoice = invoke(
        payee,
        4,
        "wallet_create_invoice",
        json!({ "amount_msat": 1000, "expiry_secs": 600 }),
        wait,
    )
    .await
    .unwrap();
    println!("invoice: {invoice}");
    let hash = invoice["payment_hash"].as_str().unwrap().to_owned();

    let pending = invoke(
        payee,
        5,
        "wallet_lookup",
        json!({ "payment_hash": hash }),
        wait,
    )
    .await
    .unwrap();
    println!("payee lookup before payment: {pending}");

    let paid = invoke(
        payer,
        4,
        "wallet_pay",
        json!({ "invoice": invoice, "amount_msat": 1000, "max_total_msat": 4000 }),
        wait,
    )
    .await
    .unwrap();
    println!("payer pay: {paid}");

    let repeat = invoke(
        payer,
        5,
        "wallet_pay",
        json!({ "invoice": invoice, "amount_msat": 1000, "max_total_msat": 4000 }),
        wait,
    )
    .await
    .unwrap();
    println!("payer repeat pay (must be a lookup): {repeat}");

    let received = invoke(
        payee,
        6,
        "wallet_wait_for_payment",
        json!({ "payment_hash": hash }),
        wait,
    )
    .await
    .unwrap();
    println!("payee received: {received}");
}
