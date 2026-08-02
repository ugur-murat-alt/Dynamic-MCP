use std::{fs, process::Command};

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn service_status_maps_a_missing_manager_to_a_stable_runtime_code() {
    let root = tempdir().expect("temporary directory");
    let config_dir = root.path().join("config");
    let runtime_dir = root.path().join("runtime");
    fs::create_dir(&config_dir).expect("config directory");
    let output = Command::new(env!("CARGO_BIN_EXE_mcp-host"))
        .env("PATH", "")
        .args([
            "--json",
            "--runtime-dir",
            runtime_dir.to_str().expect("runtime path"),
            "daemon",
            "service",
            "status",
            "--config-dir",
            config_dir.to_str().expect("config path"),
            "--manager",
            "systemd",
            "--scope",
            "user",
        ])
        .output()
        .expect("service status should run");

    assert_eq!(output.status.code(), Some(4));
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(value["error"]["code"], "SERVICE_MANAGER_UNAVAILABLE");
}
