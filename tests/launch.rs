//! No network or credential access: reject missing host context.
#[test]
fn standalone_launch_requires_host_context() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nwc-wallet"))
        .env_remove("MESH_LLM_PLUGIN_NAME")
        .env_remove("MESH_LLM_PLUGIN_ENDPOINT")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Launch this executable"));
}
