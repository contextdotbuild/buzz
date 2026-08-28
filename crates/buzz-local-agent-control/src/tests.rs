use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{json, Value};
use tempfile::TempDir;

use super::*;

const PUBKEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PUBKEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PUBKEY_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const RELEASE_ID: &str = "1111111111111111111111111111111111111111";

struct Fixture {
    _dir: TempDir,
    runtime_root: PathBuf,
    command: PathBuf,
    store: PathBuf,
    request: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let runtime_root = dir.path().join("runtime-root");
        let command = runtime_root.join(RELEASE_ID).join("bin/buzz-acp");
        fs::create_dir_all(command.parent().expect("command parent"))
            .expect("create runtime directories");
        fs::write(&command, b"#!/bin/sh\nexit 0\n").expect("write executable fixture");
        make_executable(&command);

        let store = dir.path().join(STORE_FILENAME);
        write_store(&store, &base_records());
        let request = dir.path().join("request.json");
        Self {
            _dir: dir,
            runtime_root,
            command,
            store,
            request,
        }
    }

    fn request(&self, targets: Vec<&str>) -> Value {
        json!({
            "schemaVersion": 1,
            "expectedStoreSha256": sha256(&fs::read(&self.store).expect("read store")),
            "expectedAgentCount": 2,
            "expectedDesktopPid": dead_pid(),
            "targetPubkeys": targets,
            "acpCommand": self.command.to_string_lossy(),
            "parallelism": 10,
            "envSet": {
                "BUZZ_ACP_HEARTBEAT_INTERVAL": "900",
                "BUZZ_ACP_HEARTBEAT_MODE": "schedules",
                "BUZZ_ACP_LAZY_POOL": "true",
                "BUZZ_ACP_IDLE_POOL_SLEEP": "300"
            },
            "envUnset": []
        })
    }

    fn execute(&self, request: &Value, dry_run: bool) -> Result<Receipt, ControlError> {
        fs::write(
            &self.request,
            serde_json::to_vec_pretty(request).expect("serialize request"),
        )
        .expect("write request");
        execute_with_context(
            CliOptions {
                request_path: self.request.clone(),
                store_path: self.store.clone(),
                dry_run,
            },
            &ExecutionContext {
                runtime_root: &self.runtime_root,
            },
        )
    }

    fn store_bytes(&self) -> Vec<u8> {
        fs::read(&self.store).expect("read fixture store")
    }
}

fn base_records() -> Value {
    json!([
        {
            "pubkey": PUBKEY_A,
            "name": "Alpha",
            "persona_id": "persona-alpha",
            "private_key_nsec": "nsec1-secret-alpha",
            "auth_tag": ["auth", "secret-auth-alpha"],
            "system_prompt": "secret system prompt alpha",
            "provider": "plexer",
            "model": "gpt-secret-model",
            "team_id": "engineering",
            "channel_ids": ["channel-secret-alpha"],
            "persona_source_version": "persona-v1",
            "acp_command": "/previous/bin/buzz-acp",
            "parallelism": 10,
            "env_vars": {
                "ARBITRARY_SECRET": "env-secret-alpha",
                "BUZZ_ACP_HEARTBEAT_MODE": "previous"
            },
            "updated_at": "2026-01-01T00:00:00Z",
            "unknown_extension": {"preserve": [3, 2, 1]}
        },
        {
            "pubkey": "",
            "name": "Keyless definition",
            "slug": "keyless-definition",
            "system_prompt": "definition prompt",
            "unknown_definition_field": true,
            "updated_at": "2026-01-01T00:00:00Z"
        },
        {
            "pubkey": PUBKEY_B,
            "name": "Beta",
            "persona_id": "persona-beta",
            "private_key_nsec": "nsec1-secret-beta",
            "auth_tag": ["auth", "secret-auth-beta"],
            "system_prompt": "secret system prompt beta",
            "provider": "plexer",
            "model": "gpt-other-secret-model",
            "team_id": "product",
            "channel_ids": ["channel-secret-beta"],
            "acp_command": "/previous/bin/buzz-acp",
            "parallelism": 10,
            "env_vars": {"ARBITRARY_SECRET": "env-secret-beta"},
            "updated_at": "2026-01-01T00:00:00Z",
            "unknown_extension": {"preserve": "beta"}
        }
    ])
}

fn write_store(path: &Path, records: &Value) {
    let mut bytes = serde_json::to_vec_pretty(records).expect("serialize store");
    bytes.push(b'\n');
    fs::write(path, bytes).expect("write store");
    set_mode(path, 0o600);
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn make_executable(path: &Path) {
    set_mode(path, 0o700);
}

fn dead_pid() -> u32 {
    let mut child = Command::new("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id();
    child.wait().expect("wait for short-lived child");
    pid
}

fn error_code(result: Result<Receipt, ControlError>) -> &'static str {
    result.expect_err("operation must fail").code
}

fn parse_store(fixture: &Fixture) -> Value {
    serde_json::from_slice(&fixture.store_bytes()).expect("parse fixture store")
}

#[test]
fn happy_path_preserves_order_unknown_fields_and_all_protected_data() {
    let fixture = Fixture::new();
    let before_bytes = fixture.store_bytes();
    let before: Value = serde_json::from_slice(&before_bytes).expect("parse before");
    let before_records = before.as_array().expect("before array");
    let mut request = fixture.request(vec![PUBKEY_A, PUBKEY_B]);
    let receipt_path = fixture._dir.path().join("control-receipt.json");
    request["receiptPath"] = json!(receipt_path);
    let receipt = fixture.execute(&request, false).expect("apply patch");

    let after = parse_store(&fixture);
    let after_records = after.as_array().expect("after array");
    assert_eq!(after_records.len(), before_records.len());
    assert_eq!(
        after_records
            .iter()
            .map(|record| record["pubkey"].as_str().expect("pubkey"))
            .collect::<Vec<_>>(),
        vec![PUBKEY_A, "", PUBKEY_B],
        "record and identity order changed"
    );
    assert_eq!(after_records[1], before_records[1]);

    for index in [0usize, 2usize] {
        for field in [
            "pubkey",
            "name",
            "persona_id",
            "private_key_nsec",
            "auth_tag",
            "system_prompt",
            "provider",
            "model",
            "team_id",
            "channel_ids",
            "unknown_extension",
        ] {
            assert_eq!(
                after_records[index].get(field),
                before_records[index].get(field),
                "protected field changed: {field}"
            );
        }
        assert_eq!(
            after_records[index]["env_vars"]["ARBITRARY_SECRET"],
            before_records[index]["env_vars"]["ARBITRARY_SECRET"]
        );
        assert_eq!(after_records[index]["acp_command"], json!(fixture.command));
        assert_eq!(after_records[index]["parallelism"], 10);
        assert_eq!(
            after_records[index]["env_vars"]["BUZZ_ACP_HEARTBEAT_INTERVAL"],
            "900"
        );
        assert_eq!(
            after_records[index]["env_vars"]["BUZZ_ACP_HEARTBEAT_MODE"],
            "schedules"
        );
        assert_eq!(
            after_records[index]["env_vars"]["BUZZ_ACP_LAZY_POOL"],
            "true"
        );
        assert_eq!(
            after_records[index]["env_vars"]["BUZZ_ACP_IDLE_POOL_SLEEP"],
            "300"
        );
    }

    assert_eq!(receipt.status, "applied");
    assert_eq!(receipt.agent_count, 2);
    assert_eq!(receipt.target_pubkeys, vec![PUBKEY_A, PUBKEY_B]);
    assert!(receipt.changed_fields.contains(&"updated_at".to_owned()));
    assert!(receipt.changed_fields.contains(&"env_vars".to_owned()));
    assert_eq!(receipt.after_sha256, sha256(&fixture.store_bytes()));
    assert!(receipt_path.is_file());
    assert_eq!(
        serde_json::from_slice::<Receipt>(&fs::read(receipt_path).expect("read receipt"))
            .expect("parse receipt"),
        receipt
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&fixture.store)
                .expect("store metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn wrong_hash_does_not_mutate() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let mut request = fixture.request(vec![PUBKEY_A]);
    request["expectedStoreSha256"] = json!("0".repeat(64));
    assert_eq!(
        error_code(fixture.execute(&request, false)),
        "store_hash_mismatch"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn live_expected_pid_does_not_mutate() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let mut request = fixture.request(vec![PUBKEY_A]);
    request["expectedDesktopPid"] = json!(std::process::id());
    assert_eq!(
        error_code(fixture.execute(&request, false)),
        "desktop_pid_alive"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[cfg(unix)]
#[test]
fn missing_expected_pid_does_not_mutate() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let mut request = fixture.request(vec![PUBKEY_A]);
    request
        .as_object_mut()
        .expect("request object")
        .remove("expectedDesktopPid");
    assert_eq!(
        error_code(fixture.execute(&request, false)),
        "missing_expected_desktop_pid"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn missing_and_duplicate_targets_do_not_mutate() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let missing = fixture.request(vec![PUBKEY_A, PUBKEY_C]);
    assert_eq!(
        error_code(fixture.execute(&missing, false)),
        "target_not_found"
    );
    assert_eq!(
        fixture.store_bytes(),
        before,
        "partial target write occurred"
    );

    let duplicate = fixture.request(vec![PUBKEY_A, PUBKEY_A]);
    assert_eq!(
        error_code(fixture.execute(&duplicate, false)),
        "duplicate_target_pubkey"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn duplicate_store_pubkey_and_wrong_count_do_not_mutate() {
    let fixture = Fixture::new();
    let mut records = base_records();
    records[2]["pubkey"] = json!(PUBKEY_A);
    write_store(&fixture.store, &records);
    let duplicate_bytes = fixture.store_bytes();
    let duplicate = fixture.request(vec![PUBKEY_A]);
    assert_eq!(
        error_code(fixture.execute(&duplicate, false)),
        "duplicate_store_pubkey"
    );
    assert_eq!(fixture.store_bytes(), duplicate_bytes);

    write_store(&fixture.store, &base_records());
    let before = fixture.store_bytes();
    let mut wrong_count = fixture.request(vec![PUBKEY_A]);
    wrong_count["expectedAgentCount"] = json!(3);
    assert_eq!(
        error_code(fixture.execute(&wrong_count, false)),
        "agent_count_mismatch"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn invalid_noncanonical_and_symlink_escape_commands_are_rejected() {
    let relative_fixture = Fixture::new();
    let mut relative = relative_fixture.request(vec![PUBKEY_A]);
    relative["acpCommand"] = json!("relative/buzz-acp");
    assert_eq!(
        error_code(relative_fixture.execute(&relative, false)),
        "invalid_acp_command"
    );

    let noncanonical_fixture = Fixture::new();
    let release = noncanonical_fixture.runtime_root.join(RELEASE_ID);
    let mut noncanonical = noncanonical_fixture.request(vec![PUBKEY_A]);
    noncanonical["acpCommand"] = json!(release.join("bin/../bin/buzz-acp"));
    assert_eq!(
        error_code(noncanonical_fixture.execute(&noncanonical, false)),
        "invalid_acp_command"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let escape_fixture = Fixture::new();
        let outside = escape_fixture._dir.path().join("outside-buzz-acp");
        fs::write(&outside, b"#!/bin/sh\n").expect("write outside command");
        make_executable(&outside);
        fs::remove_file(&escape_fixture.command).expect("remove command fixture");
        symlink(&outside, &escape_fixture.command).expect("create escaping symlink");
        let escape = escape_fixture.request(vec![PUBKEY_A]);
        assert_eq!(
            error_code(escape_fixture.execute(&escape, false)),
            "acp_command_symlink_escape"
        );

        let release_escape_fixture = Fixture::new();
        let release_path = release_escape_fixture.runtime_root.join(RELEASE_ID);
        fs::remove_dir_all(&release_path).expect("remove release fixture");
        let outside_release = release_escape_fixture._dir.path().join("outside-release");
        let outside_command = outside_release.join("bin/buzz-acp");
        fs::create_dir_all(outside_command.parent().expect("outside parent"))
            .expect("create outside release");
        fs::write(&outside_command, b"#!/bin/sh\n").expect("write outside release command");
        make_executable(&outside_command);
        symlink(&outside_release, &release_path).expect("symlink release outside runtime root");
        let release_escape = release_escape_fixture.request(vec![PUBKEY_A]);
        assert_eq!(
            error_code(release_escape_fixture.execute(&release_escape, false)),
            "acp_release_symlink_escape"
        );
    }
}

#[test]
fn command_must_be_commit_pinned_regular_and_executable() {
    let fixture = Fixture::new();
    let invalid_release_dir = fixture.runtime_root.join("not-a-commit").join("bin");
    fs::create_dir_all(&invalid_release_dir).expect("create invalid release");
    let invalid_release_command = invalid_release_dir.join("buzz-acp");
    fs::write(&invalid_release_command, b"#!/bin/sh\n").expect("write invalid release command");
    make_executable(&invalid_release_command);
    let mut invalid_release = fixture.request(vec![PUBKEY_A]);
    invalid_release["acpCommand"] = json!(invalid_release_command);
    assert_eq!(
        error_code(fixture.execute(&invalid_release, false)),
        "invalid_acp_release_id"
    );

    set_mode(&fixture.command, 0o600);
    let not_executable = fixture.request(vec![PUBKEY_A]);
    assert_eq!(
        error_code(fixture.execute(&not_executable, false)),
        "acp_command_not_executable"
    );
}

#[test]
fn broad_mode_and_symlink_store_are_rejected() {
    let broad_fixture = Fixture::new();
    let before = broad_fixture.store_bytes();
    set_mode(&broad_fixture.store, 0o644);
    let broad = broad_fixture.request(vec![PUBKEY_A]);
    assert_eq!(
        error_code(broad_fixture.execute(&broad, false)),
        "store_permissions_too_broad"
    );
    assert_eq!(broad_fixture.store_bytes(), before);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let symlink_fixture = Fixture::new();
        let actual = symlink_fixture._dir.path().join("actual-store.json");
        fs::rename(&symlink_fixture.store, &actual).expect("move actual store");
        symlink(&actual, &symlink_fixture.store).expect("symlink store");
        let request = symlink_fixture.request(vec![PUBKEY_A]);
        assert_eq!(
            error_code(symlink_fixture.execute(&request, false)),
            "invalid_store_file_type"
        );
    }
}

#[test]
fn invalid_env_key_empty_value_and_parallelism_are_rejected() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();

    let mut bad_key = fixture.request(vec![PUBKEY_A]);
    bad_key["envSet"] = json!({"UNBOUNDED_SECRET": "do-not-touch"});
    assert_eq!(
        error_code(fixture.execute(&bad_key, false)),
        "invalid_env_key"
    );

    let mut bad_unset = fixture.request(vec![PUBKEY_A]);
    bad_unset["envSet"] = json!({});
    bad_unset["envUnset"] = json!(["UNBOUNDED_SECRET"]);
    assert_eq!(
        error_code(fixture.execute(&bad_unset, false)),
        "invalid_env_key"
    );

    let mut empty_value = fixture.request(vec![PUBKEY_A]);
    empty_value["envSet"] = json!({"BUZZ_ACP_HEARTBEAT_INTERVAL": ""});
    assert_eq!(
        error_code(fixture.execute(&empty_value, false)),
        "empty_env_value"
    );

    let mut parallelism = fixture.request(vec![PUBKEY_A]);
    parallelism["parallelism"] = json!(9);
    assert_eq!(
        error_code(fixture.execute(&parallelism, false)),
        "invalid_parallelism"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn malformed_target_env_vars_does_not_partially_write_multiple_targets() {
    let fixture = Fixture::new();
    let mut records = base_records();
    records[2]["env_vars"] = json!("not-an-object");
    write_store(&fixture.store, &records);
    let before = fixture.store_bytes();
    let request = fixture.request(vec![PUBKEY_A, PUBKEY_B]);
    assert_eq!(
        error_code(fixture.execute(&request, false)),
        "invalid_store_env_vars"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn receipt_is_redacted_and_lists_only_allowed_change_names() {
    let fixture = Fixture::new();
    let request = fixture.request(vec![PUBKEY_A]);
    let receipt = fixture.execute(&request, false).expect("apply patch");
    let output = serde_json::to_string(&receipt).expect("serialize receipt");
    for forbidden in [
        "nsec1-secret-alpha",
        "secret-auth-alpha",
        "secret system prompt alpha",
        "gpt-secret-model",
        "persona-alpha",
        "channel-secret-alpha",
        "env-secret-alpha",
        "private_key_nsec",
        "auth_tag",
        "system_prompt",
        "provider",
        "model",
        "persona_id",
        "team_id",
        "channel_ids",
        "ARBITRARY_SECRET",
    ] {
        assert!(!output.contains(forbidden), "receipt leaked {forbidden}");
    }
    assert_eq!(
        receipt.changed_env_keys,
        vec![
            "BUZZ_ACP_HEARTBEAT_INTERVAL",
            "BUZZ_ACP_HEARTBEAT_MODE",
            "BUZZ_ACP_IDLE_POOL_SLEEP",
            "BUZZ_ACP_LAZY_POOL",
        ]
    );
}

#[test]
fn dry_run_reports_candidate_without_writing_store_or_receipt() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let receipt_path = fixture._dir.path().join("dry-run-receipt.json");
    let mut request = fixture.request(vec![PUBKEY_A]);
    request["receiptPath"] = json!(receipt_path);
    let receipt = fixture.execute(&request, true).expect("dry run");
    assert_eq!(receipt.status, "dry_run");
    assert_ne!(receipt.after_sha256, receipt.actual_before_sha256);
    assert_eq!(fixture.store_bytes(), before);
    assert!(!receipt_path.exists());
}
