use std::{fs, process::Command};

use serde_json::{json, Value};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_buzz-local-agent-control"))
}

#[test]
fn missing_arguments_emit_one_structured_error() {
    let output = binary().output().expect("run control binary");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).expect("structured stderr JSON");
    assert_eq!(error["schemaVersion"], 1);
    assert_eq!(error["status"], "error");
    assert_eq!(error["code"], "invalid_cli_arguments");
}

#[test]
fn invalid_request_error_never_echoes_unknown_values() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let request_path = dir.path().join("request.json");
    let store_path = dir.path().join("managed-agents.json");
    let secret = "must-not-appear-in-stderr";
    fs::write(
        &request_path,
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "unexpectedField": secret
        }))
        .expect("serialize invalid request"),
    )
    .expect("write request");

    let output = binary()
        .args(["--request", request_path.to_str().expect("request path")])
        .args(["--store", store_path.to_str().expect("store path")])
        .output()
        .expect("run control binary");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(!stderr.contains(secret));
    let error: Value = serde_json::from_str(&stderr).expect("structured stderr JSON");
    assert_eq!(error["code"], "invalid_request_json");
}
