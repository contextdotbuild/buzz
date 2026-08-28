use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{json, Value};
use tempfile::TempDir;

use super::*;

const RELEASE_ID: &str = "1111111111111111111111111111111111111111";
const SOURCE_TREE: &str = "2222222222222222222222222222222222222222";
const ROLLBACK_TEST_RELEASE_ID: &str = "3333333333333333333333333333333333333333";
const ROLLBACK_TEST_SOURCE_TREE: &str = "4444444444444444444444444444444444444444";
const ARTIFACT_MODE: &str = "0555";

struct Fixture {
    _dir: TempDir,
    runtime_root: PathBuf,
    release_root: PathBuf,
    wrapper: PathBuf,
    libexec: PathBuf,
    mcp: PathBuf,
    manifest: PathBuf,
    desktop_executable: PathBuf,
    store: PathBuf,
    request: PathBuf,
    owner: String,
    manifest_hash: String,
    wrapper_hash: String,
    wrapper_size: u64,
    libexec_hash: String,
    libexec_size: u64,
    mcp_hash: String,
    mcp_size: u64,
    rollback_wrapper: PathBuf,
    rollback_manifest_hash: String,
    rollback_wrapper_hash: String,
    rollback_wrapper_size: u64,
    rollback_libexec_hash: String,
    rollback_libexec_size: u64,
    pubkeys: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let root = fs::canonicalize(dir.path()).expect("canonical fixture root");
        let runtime_root = root.join("runtime-root");
        let release_root = runtime_root.join(RELEASE_ID);
        let wrapper = release_root.join("bin/buzz-acp");
        let libexec = release_root.join("libexec/buzz-acp");
        let mcp = release_root.join("bin/buzz-dev-mcp");
        fs::create_dir_all(wrapper.parent().expect("wrapper parent"))
            .expect("create wrapper directory");
        fs::create_dir_all(libexec.parent().expect("libexec parent"))
            .expect("create libexec directory");
        let wrapper_bytes = b"#!/bin/sh\nexec ../libexec/buzz-acp \"$@\"\n";
        let libexec_bytes = b"fixture immutable runtime executable bytes";
        let mcp_bytes = b"fixture immutable MCP executable bytes";
        fs::write(&wrapper, wrapper_bytes).expect("write wrapper fixture");
        fs::write(&libexec, libexec_bytes).expect("write libexec fixture");
        fs::write(&mcp, mcp_bytes).expect("write MCP fixture");
        set_mode(&wrapper, 0o555);
        set_mode(&libexec, 0o555);
        set_mode(&mcp, 0o555);

        let owner = current_owner();
        let wrapper_hash = sha256(wrapper_bytes);
        let libexec_hash = sha256(libexec_bytes);
        let mcp_hash = sha256(mcp_bytes);
        let wrapper_size = u64::try_from(wrapper_bytes.len()).expect("wrapper size");
        let libexec_size = u64::try_from(libexec_bytes.len()).expect("libexec size");
        let mcp_size = u64::try_from(mcp_bytes.len()).expect("MCP size");
        let manifest = release_root.join("MANIFEST.json");
        let manifest_value = json!({
            "schema": 1,
            "source": {"commit": RELEASE_ID, "tree": SOURCE_TREE},
            "build": {
                "profile": "release",
                "target": "aarch64-apple-darwin",
                "toolchain": "rustc 1.93.0"
            },
            "desktop_contract": {
                "acp_command": wrapper.to_string_lossy(),
                "mcp_command": mcp.to_string_lossy(),
                "environment": EnvironmentContract::Forward.env_set(),
                "unchanged_desktop": "/Applications/Buzz.app 0.5.19",
                "unchanged_global_cli": "/Users/timi/.local/bin/buzz"
            },
            "artifacts": [
                {"path": "bin/buzz-acp", "sha256": wrapper_hash, "size": wrapper_size},
                {"path": "libexec/buzz-acp", "sha256": libexec_hash, "size": libexec_size},
                {"path": "bin/buzz-dev-mcp", "sha256": mcp_hash, "size": mcp_size}
            ]
        });
        let mut manifest_bytes =
            serde_json::to_vec_pretty(&manifest_value).expect("serialize manifest");
        manifest_bytes.push(b'\n');
        fs::write(&manifest, &manifest_bytes).expect("write manifest");
        set_mode(&manifest, 0o444);
        let manifest_hash = sha256(&manifest_bytes);

        let rollback_release_root = runtime_root.join(ROLLBACK_TEST_RELEASE_ID);
        let rollback_wrapper = rollback_release_root.join("bin/buzz-acp");
        let rollback_libexec = rollback_release_root.join("libexec/buzz-acp");
        fs::create_dir_all(rollback_wrapper.parent().expect("rollback wrapper parent"))
            .expect("create rollback wrapper directory");
        fs::create_dir_all(rollback_libexec.parent().expect("rollback libexec parent"))
            .expect("create rollback libexec directory");
        let rollback_libexec_bytes = b"fixture immutable rollback executable bytes";
        fs::write(&rollback_wrapper, wrapper_bytes).expect("write rollback wrapper fixture");
        fs::write(&rollback_libexec, rollback_libexec_bytes)
            .expect("write rollback libexec fixture");
        set_mode(&rollback_wrapper, 0o555);
        set_mode(&rollback_libexec, 0o555);
        let rollback_wrapper_hash = sha256(wrapper_bytes);
        let rollback_libexec_hash = sha256(rollback_libexec_bytes);
        let rollback_wrapper_size =
            u64::try_from(wrapper_bytes.len()).expect("rollback wrapper size");
        let rollback_libexec_size =
            u64::try_from(rollback_libexec_bytes.len()).expect("rollback libexec size");
        let rollback_manifest = rollback_release_root.join("MANIFEST.json");
        let rollback_manifest_value = json!({
            "schema": 1,
            "source": {
                "commit": ROLLBACK_TEST_RELEASE_ID,
                "tree": ROLLBACK_TEST_SOURCE_TREE
            },
            "build": {
                "profile": "release",
                "target": "aarch64-apple-darwin",
                "toolchain": "rustc 1.95.0"
            },
            "desktop_contract": {
                "acp_command": rollback_wrapper.to_string_lossy(),
                "environment": EnvironmentContract::Rollback.env_set(),
                "unchanged_desktop": "/Applications/Buzz.app 0.5.19",
                "unchanged_global_cli": "/Users/timi/.local/bin/buzz"
            },
            "artifacts": [
                {
                    "path": "bin/buzz-acp",
                    "sha256": rollback_wrapper_hash,
                    "size": rollback_wrapper_size
                },
                {
                    "path": "libexec/buzz-acp",
                    "sha256": rollback_libexec_hash,
                    "size": rollback_libexec_size
                }
            ]
        });
        let mut rollback_manifest_bytes = serde_json::to_vec_pretty(&rollback_manifest_value)
            .expect("serialize rollback manifest");
        rollback_manifest_bytes.push(b'\n');
        fs::write(&rollback_manifest, &rollback_manifest_bytes).expect("write rollback manifest");
        set_mode(&rollback_manifest, 0o444);
        let rollback_manifest_hash = sha256(&rollback_manifest_bytes);

        let desktop_executable = root.join("buzz-desktop");
        fs::write(&desktop_executable, b"desktop fixture").expect("write desktop fixture");
        set_mode(&desktop_executable, 0o555);

        let pubkeys = test_pubkeys();
        let store = root.join(STORE_FILENAME);
        write_store(&store, &base_records(&pubkeys));
        let request = root.join("request.json");
        Self {
            _dir: dir,
            runtime_root,
            release_root,
            wrapper,
            libexec,
            mcp,
            manifest,
            desktop_executable,
            store,
            request,
            owner,
            manifest_hash,
            wrapper_hash,
            wrapper_size,
            libexec_hash,
            libexec_size,
            mcp_hash,
            mcp_size,
            rollback_wrapper,
            rollback_manifest_hash,
            rollback_wrapper_hash,
            rollback_wrapper_size,
            rollback_libexec_hash,
            rollback_libexec_size,
            pubkeys,
        }
    }

    fn request(&self) -> Value {
        json!({
            "schemaVersion": 1,
            "expectedStoreSha256": sha256(&self.store_bytes()),
            "expectedAgentCount": CANONICAL_AGENT_COUNT,
            "expectedDesktopPid": dead_pid(),
            "targetPubkeys": self.pubkeys,
            "acpCommand": self.wrapper.to_string_lossy(),
            "mcpCommand": self.mcp.to_string_lossy(),
            "expectedReleaseId": RELEASE_ID,
            "expectedSourceTree": SOURCE_TREE,
            "expectedManifestSha256": self.manifest_hash,
            "expectedAcpCommandSha256": self.wrapper_hash,
            "expectedAcpCommandSize": self.wrapper_size,
            "expectedLibexecSha256": self.libexec_hash,
            "expectedLibexecSize": self.libexec_size,
            "expectedMcpCommandSha256": self.mcp_hash,
            "expectedMcpCommandSize": self.mcp_size,
            "expectedArtifactOwner": self.owner,
            "expectedArtifactMode": ARTIFACT_MODE,
            "parallelism": REQUIRED_PARALLELISM,
            "envSet": EnvironmentContract::Forward.env_set(),
            "envUnset": []
        })
    }

    fn rollback_request(&self) -> Value {
        json!({
            "schemaVersion": 1,
            "expectedStoreSha256": sha256(&self.store_bytes()),
            "expectedAgentCount": CANONICAL_AGENT_COUNT,
            "expectedDesktopPid": dead_pid(),
            "targetPubkeys": self.pubkeys,
            "acpCommand": self.rollback_wrapper.to_string_lossy(),
            "mcpCommand": "buzz-dev-mcp",
            "expectedReleaseId": ROLLBACK_TEST_RELEASE_ID,
            "expectedSourceTree": ROLLBACK_TEST_SOURCE_TREE,
            "expectedManifestSha256": self.rollback_manifest_hash,
            "expectedAcpCommandSha256": self.rollback_wrapper_hash,
            "expectedAcpCommandSize": self.rollback_wrapper_size,
            "expectedLibexecSha256": self.rollback_libexec_hash,
            "expectedLibexecSize": self.rollback_libexec_size,
            "expectedMcpCommandSha256": "",
            "expectedMcpCommandSize": 0,
            "expectedArtifactOwner": self.owner,
            "expectedArtifactMode": ARTIFACT_MODE,
            "parallelism": REQUIRED_PARALLELISM,
            "envSet": EnvironmentContract::Rollback.env_set(),
            "envUnset": EnvironmentContract::Rollback.env_unset()
        })
    }

    fn context<'a>(&'a self, inspector: &'a dyn ProcessInspector) -> ExecutionContext<'a> {
        self.context_with(inspector, &self.owner)
    }

    fn context_with<'a>(
        &'a self,
        inspector: &'a dyn ProcessInspector,
        owner: &'a str,
    ) -> ExecutionContext<'a> {
        ExecutionContext {
            runtime_root: &self.runtime_root,
            canonical_store_path: &self.store,
            desktop_executable: &self.desktop_executable,
            expected_agent_count: CANONICAL_AGENT_COUNT,
            forward_artifacts: ArtifactContract {
                release_id: RELEASE_ID,
                source_tree: SOURCE_TREE,
                manifest_sha256: &self.manifest_hash,
                command_sha256: &self.wrapper_hash,
                command_size: self.wrapper_size,
                libexec_sha256: &self.libexec_hash,
                libexec_size: self.libexec_size,
                mcp: McpContract::RuntimeArtifact {
                    sha256: &self.mcp_hash,
                    size: self.mcp_size,
                },
                owner,
                mode: ARTIFACT_MODE,
                toolchain: "rustc 1.93.0",
                environment: EnvironmentContract::Forward,
            },
            rollback_artifacts: ArtifactContract {
                release_id: ROLLBACK_TEST_RELEASE_ID,
                source_tree: ROLLBACK_TEST_SOURCE_TREE,
                manifest_sha256: &self.rollback_manifest_hash,
                command_sha256: &self.rollback_wrapper_hash,
                command_size: self.rollback_wrapper_size,
                libexec_sha256: &self.rollback_libexec_hash,
                libexec_size: self.rollback_libexec_size,
                mcp: McpContract::BundledCommand,
                owner,
                mode: ARTIFACT_MODE,
                toolchain: "rustc 1.95.0",
                environment: EnvironmentContract::Rollback,
            },
            process_inspector: inspector,
        }
    }

    fn execute(
        &self,
        request: &Value,
        dry_run: bool,
        inspector: &dyn ProcessInspector,
    ) -> Result<Receipt, ControlError> {
        self.execute_with_paths(request, dry_run, inspector, &self.store, &self.store)
    }

    fn execute_with_paths(
        &self,
        request: &Value,
        dry_run: bool,
        inspector: &dyn ProcessInspector,
        store_path: &Path,
        canonical_store_path: &Path,
    ) -> Result<Receipt, ControlError> {
        fs::write(
            &self.request,
            serde_json::to_vec_pretty(request).expect("serialize request"),
        )
        .expect("write request");
        let mut context = self.context(inspector);
        context.canonical_store_path = canonical_store_path;
        execute_with_context(
            CliOptions {
                request_path: self.request.clone(),
                store_path: store_path.to_owned(),
                dry_run,
            },
            &context,
        )
    }

    fn store_bytes(&self) -> Vec<u8> {
        fs::read(&self.store).expect("read fixture store")
    }
}

#[derive(Default)]
struct CountingInspector {
    calls: Cell<usize>,
    fail_on_call: Option<usize>,
    mutate_on_call: Option<(usize, PathBuf, Vec<u8>, u32)>,
}

impl CountingInspector {
    fn fail_on(call: usize) -> Self {
        Self {
            calls: Cell::new(0),
            fail_on_call: Some(call),
            mutate_on_call: None,
        }
    }

    fn mutate_on(call: usize, path: PathBuf, bytes: Vec<u8>, mode: u32) -> Self {
        Self {
            calls: Cell::new(0),
            fail_on_call: None,
            mutate_on_call: Some((call, path, bytes, mode)),
        }
    }
}

impl ProcessInspector for CountingInspector {
    fn ensure_desktop_absent(&self, _executable: &Path) -> Result<(), ControlError> {
        let call = self.calls.get() + 1;
        self.calls.set(call);
        if let Some((target_call, path, bytes, mode)) = &self.mutate_on_call {
            if call == *target_call {
                overwrite_artifact(path, bytes, *mode);
            }
        }
        if self.fail_on_call == Some(call) {
            return Err(ControlError::new(
                "desktop_process_alive",
                "test Desktop process appeared at final fence",
            ));
        }
        Ok(())
    }
}

fn test_pubkeys() -> Vec<String> {
    (1..=CANONICAL_AGENT_COUNT)
        .map(|index| format!("{index:064x}"))
        .collect()
}

fn base_records(pubkeys: &[String]) -> Value {
    let mut records = Vec::new();
    for (index, pubkey) in pubkeys.iter().enumerate() {
        let mut record = json!({
            "pubkey": pubkey,
            "name": format!("Fleet identity {index}"),
            "persona_id": format!("persona-{index}"),
            "private_key_nsec": format!("nsec1-secret-{index}"),
            "auth_tag": ["auth", format!("secret-auth-{index}")],
            "system_prompt": format!("secret system prompt {index}"),
            "provider": "plexer",
            "model": format!("gpt-secret-model-{index}"),
            "team_id": format!("team-{index}"),
            "channel_ids": [format!("channel-secret-{index}")],
            "acp_command": "/previous/bin/buzz-acp",
            "mcp_command": "buzz-dev-mcp",
            "parallelism": 10,
            "updated_at": format!("2026-01-{index:02}T00:00:00Z"),
            "unknown_extension": {"preserve": [index, 2, 1]}
        });
        if index % 2 == 0 {
            record["env_vars"] = json!({
                "ARBITRARY_SECRET": format!("env-secret-{index}"),
                "BUZZ_ACP_HEARTBEAT_MODE": "previous"
            });
        }
        records.push(record);
        records.push(json!({
            "pubkey": "",
            "name": format!("Keyless definition {index}"),
            "system_prompt": format!("definition prompt {index}"),
            "updated_at": format!("2025-12-{index:02}T00:00:00Z"),
            "unknown_definition_field": true
        }));
    }
    records.push(json!({
        "pubkey": "",
        "name": "Extra keyless definition A",
        "unknown_definition_field": "preserve"
    }));
    records.push(json!({
        "pubkey": "",
        "name": "Extra keyless definition B",
        "unknown_definition_field": [1, 2, 3]
    }));
    records.push(json!({
        "pubkey": "",
        "name": "Extra keyless definition C",
        "unknown_definition_field": {"nested": true}
    }));
    Value::Array(records)
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

fn overwrite_artifact(path: &Path, bytes: &[u8], final_mode: u32) {
    set_mode(path, 0o755);
    fs::write(path, bytes).expect("overwrite artifact fixture");
    set_mode(path, final_mode);
}

#[cfg(unix)]
fn current_owner() -> String {
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .expect("owner lookup")
        .expect("current owner")
        .name
}

#[cfg(not(unix))]
fn current_owner() -> String {
    "test-owner".to_owned()
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
fn fleet_wide_apply_preserves_every_unapproved_field_and_keyless_record() {
    let fixture = Fixture::new();
    let before: Value = serde_json::from_slice(&fixture.store_bytes()).expect("parse before");
    let before_records = before.as_array().expect("before array");
    let request = fixture.request();
    let inspector = CountingInspector::default();
    let receipt = fixture
        .execute(&request, false, &inspector)
        .expect("apply fleet patch");
    let after = parse_store(&fixture);
    let after_records = after.as_array().expect("after array");
    assert_eq!(inspector.calls.get(), 2, "both process fences must run");
    assert_eq!(after_records.len(), before_records.len());

    for (before_record, after_record) in before_records.iter().zip(after_records) {
        if before_record["pubkey"] == "" {
            assert_eq!(after_record, before_record, "keyless record changed");
            continue;
        }
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
            "updated_at",
            "unknown_extension",
        ] {
            assert_eq!(
                after_record.get(field),
                before_record.get(field),
                "protected field changed: {field}"
            );
        }
        assert_eq!(after_record["acp_command"], json!(fixture.wrapper));
        assert_eq!(after_record["mcp_command"], json!(fixture.mcp));
        assert_eq!(after_record["parallelism"], REQUIRED_PARALLELISM);
        for (key, value) in EnvironmentContract::Forward.env_set() {
            assert_eq!(after_record["env_vars"][key], value);
        }
        if let Some(secret) = before_record
            .get("env_vars")
            .and_then(Value::as_object)
            .and_then(|env| env.get("ARBITRARY_SECRET"))
        {
            assert_eq!(after_record["env_vars"]["ARBITRARY_SECRET"], *secret);
        }
    }
    assert_eq!(receipt.agent_count, CANONICAL_AGENT_COUNT);
    assert_eq!(receipt.target_pubkeys, fixture.pubkeys);
    assert_eq!(receipt.after_sha256, sha256(&fixture.store_bytes()));
    assert!(!receipt.changed_fields.contains(&"updated_at".to_owned()));
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
fn dry_run_and_apply_have_identical_candidate_hashes_without_wall_clock_output() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let request = fixture.request();
    let first_inspector = CountingInspector::default();
    let first = fixture
        .execute(&request, true, &first_inspector)
        .expect("first dry run");
    let second_inspector = CountingInspector::default();
    let second = fixture
        .execute(&request, true, &second_inspector)
        .expect("second dry run");
    assert_eq!(first, second, "dry-run receipt depends on wall clock");
    assert_eq!(fixture.store_bytes(), before);

    let apply_inspector = CountingInspector::default();
    let applied = fixture
        .execute(&request, false, &apply_inspector)
        .expect("apply same candidate");
    assert_eq!(first.after_sha256, applied.after_sha256);
    assert_eq!(applied.after_sha256, sha256(&fixture.store_bytes()));
}

#[test]
fn approved_inverse_is_deterministic_and_preserves_runtime_environment() {
    let fixture = Fixture::new();
    let forward_request = fixture.request();
    fixture
        .execute(&forward_request, false, &CountingInspector::default())
        .expect("apply approved forward contract");
    let before_inverse = parse_store(&fixture);

    let rollback_request = fixture.rollback_request();
    let first = fixture
        .execute(&rollback_request, true, &CountingInspector::default())
        .expect("first inverse dry run");
    let second = fixture
        .execute(&rollback_request, true, &CountingInspector::default())
        .expect("second inverse dry run");
    assert_eq!(first, second, "inverse candidate depends on wall clock");

    let applied = fixture
        .execute(&rollback_request, false, &CountingInspector::default())
        .expect("apply approved inverse contract");
    assert_eq!(first.after_sha256, applied.after_sha256);
    assert_eq!(applied.after_sha256, sha256(&fixture.store_bytes()));
    assert_eq!(
        applied.changed_fields,
        vec!["acp_command".to_owned(), "mcp_command".to_owned()]
    );
    assert!(applied.changed_env_keys.is_empty());

    let after_inverse = parse_store(&fixture);
    for (before_record, after_record) in before_inverse
        .as_array()
        .expect("before inverse records")
        .iter()
        .zip(after_inverse.as_array().expect("after inverse records"))
    {
        if before_record["pubkey"] == "" {
            assert_eq!(after_record, before_record, "keyless record changed");
            continue;
        }
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
            "parallelism",
            "updated_at",
            "unknown_extension",
        ] {
            assert_eq!(
                after_record.get(field),
                before_record.get(field),
                "inverse changed protected field: {field}"
            );
        }
        assert_ne!(after_record["acp_command"], before_record["acp_command"]);
        assert_eq!(after_record["acp_command"], json!(fixture.rollback_wrapper));
        assert_ne!(after_record["mcp_command"], before_record["mcp_command"]);
        assert_eq!(after_record["mcp_command"], json!("buzz-dev-mcp"));
        for key in [
            "BUZZ_ACP_HEARTBEAT_INTERVAL",
            "BUZZ_ACP_HEARTBEAT_MODE",
            "BUZZ_ACP_LAZY_POOL",
            "BUZZ_ACP_IDLE_POOL_SLEEP",
        ] {
            assert_eq!(
                after_record["env_vars"][key], before_record["env_vars"][key],
                "inverse changed retained heartbeat value: {key}"
            );
        }
        assert_eq!(
            after_record["env_vars"].get("ARBITRARY_SECRET"),
            before_record["env_vars"].get("ARBITRARY_SECRET")
        );
    }
}

#[test]
fn mixed_forward_and_inverse_contracts_are_rejected_without_mutation() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();

    let mut mixed = fixture.request();
    mixed["expectedLibexecSha256"] = json!(fixture.rollback_libexec_hash);
    mixed["expectedLibexecSize"] = json!(fixture.rollback_libexec_size);
    assert_eq!(
        error_code(fixture.execute(&mixed, false, &CountingInspector::default())),
        "artifact_contract_mismatch"
    );
    assert_eq!(fixture.store_bytes(), before);

    let mut unknown = fixture.rollback_request();
    unknown["expectedReleaseId"] = json!("f".repeat(40));
    assert_eq!(
        error_code(fixture.execute(&unknown, false, &CountingInspector::default())),
        "artifact_contract_mismatch"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn production_inverse_contract_is_exactly_the_approved_immutable_release() {
    let inverse = production_rollback_artifacts();
    assert_eq!(
        inverse.release_id,
        "0fe6a54b28195be7e2a188f800a0427b7b383513"
    );
    assert_eq!(
        inverse.source_tree,
        "efd70f586e13086868c04842404f313cf6ff2144"
    );
    assert_eq!(
        inverse.manifest_sha256,
        "f6e974d9ce1429be95fea77ca600b26c695bb1b549309760faa7a5647d7ded77"
    );
    assert_eq!(
        inverse.command_sha256,
        "8d2720ddde69d25a0d21c28bdd1308cf524243d8cdb86781965a7ade98858745"
    );
    assert_eq!(inverse.command_size, 184);
    assert_eq!(
        inverse.libexec_sha256,
        "ff3df3caaa8a8b69f5cd6307054f4d76abc8eaee5cb3ce91f9fa120e5a0e9ffe"
    );
    assert_eq!(inverse.libexec_size, 13_941_968);
    assert_eq!(inverse.owner, "timi");
    assert_eq!(inverse.mode, "0555");
    assert_eq!(inverse.toolchain, "rustc 1.95.0");
    assert!(matches!(inverse.mcp, McpContract::BundledCommand));
    assert_eq!(
        inverse.environment.env_set(),
        BTreeMap::from([
            ("BUZZ_ACP_HEARTBEAT_INTERVAL".to_owned(), "900".to_owned()),
            ("BUZZ_ACP_HEARTBEAT_MODE".to_owned(), "schedules".to_owned()),
            ("BUZZ_ACP_LAZY_POOL".to_owned(), "true".to_owned()),
            ("BUZZ_ACP_IDLE_POOL_SLEEP".to_owned(), "300".to_owned())
        ])
    );
    assert!(inverse.environment.env_unset().is_empty());
}

#[test]
fn receipt_path_is_a_strict_schema_error_even_when_it_symlinks_to_store() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let mut request = fixture.request();
    let alias = fixture
        .store
        .parent()
        .expect("store parent")
        .join("receipt-alias.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fixture.store, &alias).expect("store receipt alias");
    #[cfg(not(unix))]
    fs::write(&alias, b"alias fixture").expect("receipt fixture");
    request["receiptPath"] = json!(alias);
    let inspector = CountingInspector::default();
    assert_eq!(
        error_code(fixture.execute(&request, false, &inspector)),
        "invalid_request_json"
    );
    assert_eq!(inspector.calls.get(), 0);
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn target_set_must_equal_all_nine_keyed_store_identities() {
    let fixture = Fixture::new();
    let before = fixture.store_bytes();
    let mut wrong_count = fixture.request();
    wrong_count["expectedAgentCount"] = json!(8);
    assert_eq!(
        error_code(fixture.execute(&wrong_count, false, &CountingInspector::default())),
        "invalid_expected_agent_count"
    );
    assert_eq!(fixture.store_bytes(), before);

    let mut request = fixture.request();
    request["targetPubkeys"][0] = json!("f".repeat(64));
    let inspector = CountingInspector::default();
    assert_eq!(
        error_code(fixture.execute(&request, false, &inspector)),
        "target_set_mismatch"
    );
    assert_eq!(fixture.store_bytes(), before);

    let mut records = base_records(&fixture.pubkeys);
    records[0]["pubkey"] = json!("A".repeat(64));
    write_store(&fixture.store, &records);
    let invalid_before = fixture.store_bytes();
    let invalid_request = fixture.request();
    let inspector = CountingInspector::default();
    assert_eq!(
        error_code(fixture.execute(&invalid_request, false, &inspector)),
        "invalid_store_pubkey"
    );
    assert_eq!(fixture.store_bytes(), invalid_before);
}

#[test]
fn canonical_store_path_is_exact_and_rejects_intermediate_symlinks() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let other = fixture
        .store
        .parent()
        .expect("store parent")
        .join("other")
        .join(STORE_FILENAME);
    fs::create_dir_all(other.parent().expect("other parent")).expect("other directory");
    fs::write(&other, fixture.store_bytes()).expect("other store");
    set_mode(&other, 0o600);
    let inspector = CountingInspector::default();
    assert_eq!(
        error_code(fixture.execute_with_paths(&request, false, &inspector, &other, &fixture.store)),
        "invalid_store_path"
    );

    #[cfg(unix)]
    {
        let real_parent = fixture
            .store
            .parent()
            .expect("store parent")
            .join("real-agent-dir");
        fs::create_dir_all(&real_parent).expect("real agent dir");
        let real_store = real_parent.join(STORE_FILENAME);
        fs::write(&real_store, fixture.store_bytes()).expect("real store");
        set_mode(&real_store, 0o600);
        let alias_parent = fixture
            .store
            .parent()
            .expect("store parent")
            .join("alias-agent-dir");
        std::os::unix::fs::symlink(&real_parent, &alias_parent).expect("intermediate symlink");
        let alias_store = alias_parent.join(STORE_FILENAME);
        let inspector = CountingInspector::default();
        assert_eq!(
            error_code(fixture.execute_with_paths(
                &request,
                false,
                &inspector,
                &alias_store,
                &alias_store
            )),
            "invalid_store_path"
        );
    }
}

#[test]
fn immutable_release_hashes_modes_and_symlinks_are_enforced() {
    let manifest_fixture = Fixture::new();
    let manifest_before = manifest_fixture.store_bytes();
    let manifest_request = manifest_fixture.request();
    overwrite_artifact(&manifest_fixture.manifest, b"{}\n", 0o444);
    assert_eq!(
        error_code(manifest_fixture.execute(
            &manifest_request,
            false,
            &CountingInspector::default()
        )),
        "manifest_hash_mismatch"
    );
    assert_eq!(manifest_fixture.store_bytes(), manifest_before);

    let wrapper_fixture = Fixture::new();
    let wrapper_request = wrapper_fixture.request();
    let mut wrong_wrapper = fs::read(&wrapper_fixture.wrapper).expect("read wrapper");
    wrong_wrapper[0] ^= 1;
    overwrite_artifact(&wrapper_fixture.wrapper, &wrong_wrapper, 0o555);
    assert_eq!(
        error_code(wrapper_fixture.execute(&wrapper_request, false, &CountingInspector::default())),
        "acp_command_hash_mismatch"
    );

    let libexec_fixture = Fixture::new();
    let libexec_request = libexec_fixture.request();
    let mut wrong_libexec = fs::read(&libexec_fixture.libexec).expect("read libexec");
    wrong_libexec[0] ^= 1;
    overwrite_artifact(&libexec_fixture.libexec, &wrong_libexec, 0o555);
    assert_eq!(
        error_code(libexec_fixture.execute(&libexec_request, false, &CountingInspector::default())),
        "libexec_hash_mismatch"
    );

    let mcp_fixture = Fixture::new();
    let mcp_request = mcp_fixture.request();
    let mut wrong_mcp = fs::read(&mcp_fixture.mcp).expect("read MCP executable");
    wrong_mcp[0] ^= 1;
    overwrite_artifact(&mcp_fixture.mcp, &wrong_mcp, 0o555);
    assert_eq!(
        error_code(mcp_fixture.execute(&mcp_request, false, &CountingInspector::default())),
        "mcp_command_hash_mismatch"
    );

    let mode_fixture = Fixture::new();
    let mode_request = mode_fixture.request();
    set_mode(&mode_fixture.wrapper, 0o575);
    assert_eq!(
        error_code(mode_fixture.execute(&mode_request, false, &CountingInspector::default())),
        "acp_command_validation_failed"
    );

    #[cfg(unix)]
    {
        let symlink_fixture = Fixture::new();
        let symlink_request = symlink_fixture.request();
        let actual = symlink_fixture.release_root.join("bin/actual-buzz-acp");
        fs::rename(&symlink_fixture.wrapper, &actual).expect("move wrapper");
        std::os::unix::fs::symlink(&actual, &symlink_fixture.wrapper).expect("symlink wrapper");
        assert_eq!(
            error_code(symlink_fixture.execute(
                &symlink_request,
                false,
                &CountingInspector::default()
            )),
            "acp_command_validation_failed"
        );

        let libexec_symlink_fixture = Fixture::new();
        let libexec_symlink_request = libexec_symlink_fixture.request();
        let actual_libexec = libexec_symlink_fixture
            .release_root
            .join("libexec/actual-buzz-acp");
        fs::rename(&libexec_symlink_fixture.libexec, &actual_libexec).expect("move libexec");
        std::os::unix::fs::symlink(&actual_libexec, &libexec_symlink_fixture.libexec)
            .expect("symlink libexec");
        assert_eq!(
            error_code(libexec_symlink_fixture.execute(
                &libexec_symlink_request,
                false,
                &CountingInspector::default()
            )),
            "libexec_validation_failed"
        );

        let mcp_symlink_fixture = Fixture::new();
        let mcp_symlink_request = mcp_symlink_fixture.request();
        let actual_mcp = mcp_symlink_fixture
            .release_root
            .join("bin/actual-buzz-dev-mcp");
        fs::rename(&mcp_symlink_fixture.mcp, &actual_mcp).expect("move MCP executable");
        std::os::unix::fs::symlink(&actual_mcp, &mcp_symlink_fixture.mcp)
            .expect("symlink MCP executable");
        assert_eq!(
            error_code(mcp_symlink_fixture.execute(
                &mcp_symlink_request,
                false,
                &CountingInspector::default()
            )),
            "mcp_command_validation_failed"
        );

        let release_fixture = Fixture::new();
        let release_request = release_fixture.request();
        let actual_release = release_fixture
            .runtime_root
            .join("actual-release-outside-pin");
        fs::rename(&release_fixture.release_root, &actual_release).expect("move release");
        std::os::unix::fs::symlink(&actual_release, &release_fixture.release_root)
            .expect("symlink release");
        assert_eq!(
            error_code(release_fixture.execute(
                &release_request,
                false,
                &CountingInspector::default()
            )),
            "release_root_symlink"
        );
    }
}

#[cfg(unix)]
#[test]
fn artifact_owner_must_be_the_current_user() {
    let fixture = Fixture::new();
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let mut request = fixture.request();
    request["expectedArtifactOwner"] = json!("root");
    fs::write(
        &fixture.request,
        serde_json::to_vec_pretty(&request).expect("serialize request"),
    )
    .expect("write request");
    let inspector = CountingInspector::default();
    let context = fixture.context_with(&inspector, "root");
    let result = execute_with_context(
        CliOptions {
            request_path: fixture.request.clone(),
            store_path: fixture.store.clone(),
            dry_run: false,
        },
        &context,
    );
    assert_eq!(error_code(result), "artifact_owner_mismatch");
}

#[test]
fn second_process_fence_failure_and_final_artifact_change_leave_store_unchanged() {
    let process_fixture = Fixture::new();
    let process_before = process_fixture.store_bytes();
    let process_request = process_fixture.request();
    let process_inspector = CountingInspector::fail_on(2);
    assert_eq!(
        error_code(process_fixture.execute(&process_request, false, &process_inspector)),
        "desktop_process_alive"
    );
    assert_eq!(process_inspector.calls.get(), 2);
    assert_eq!(process_fixture.store_bytes(), process_before);

    let artifact_fixture = Fixture::new();
    let artifact_before = artifact_fixture.store_bytes();
    let artifact_request = artifact_fixture.request();
    let mut changed = fs::read(&artifact_fixture.wrapper).expect("read wrapper");
    changed[0] ^= 1;
    let artifact_inspector =
        CountingInspector::mutate_on(1, artifact_fixture.wrapper.clone(), changed, 0o555);
    assert_eq!(
        error_code(artifact_fixture.execute(&artifact_request, false, &artifact_inspector)),
        "acp_command_hash_mismatch"
    );
    assert_eq!(
        artifact_inspector.calls.get(),
        1,
        "final artifact fence must fail before the final process fence"
    );
    assert_eq!(artifact_fixture.store_bytes(), artifact_before);
}

#[test]
fn semantic_diff_rejects_timestamp_prompt_and_arbitrary_environment_changes() {
    let fixture = Fixture::new();
    let original = base_records(&fixture.pubkeys);
    let target_indices: Vec<usize> = (0..CANONICAL_AGENT_COUNT).map(|index| index * 2).collect();
    for mutation in [
        ("updated_at", json!("2099-01-01T00:00:00Z")),
        ("system_prompt", json!("changed prompt")),
    ] {
        let mut candidate = original.clone();
        candidate[0][mutation.0] = mutation.1;
        assert_eq!(
            validate_semantic_diff(&original, &candidate, &target_indices)
                .expect_err("protected change must fail")
                .code,
            "candidate_diff_rejected"
        );
    }
    let mut env_candidate = original.clone();
    env_candidate[0]["env_vars"]["ARBITRARY_SECRET"] = json!("changed secret");
    assert_eq!(
        validate_semantic_diff(&original, &env_candidate, &target_indices)
            .expect_err("arbitrary env change must fail")
            .code,
        "candidate_diff_rejected"
    );
}

#[test]
fn malformed_target_environment_and_invalid_parallelism_never_partially_write() {
    let fixture = Fixture::new();
    let mut records = base_records(&fixture.pubkeys);
    records[2]["env_vars"] = json!("not-an-object");
    write_store(&fixture.store, &records);
    let before = fixture.store_bytes();
    let request = fixture.request();
    assert_eq!(
        error_code(fixture.execute(&request, false, &CountingInspector::default())),
        "invalid_store_env_vars"
    );
    assert_eq!(fixture.store_bytes(), before);

    write_store(&fixture.store, &base_records(&fixture.pubkeys));
    let before = fixture.store_bytes();
    let mut parallelism = fixture.request();
    parallelism["parallelism"] = json!(9);
    assert_eq!(
        error_code(fixture.execute(&parallelism, false, &CountingInspector::default())),
        "invalid_parallelism"
    );
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn receipt_is_redacted_and_lists_only_allowed_change_names() {
    let fixture = Fixture::new();
    let receipt = fixture
        .execute(&fixture.request(), false, &CountingInspector::default())
        .expect("apply patch");
    let output = serde_json::to_string(&receipt).expect("serialize receipt");
    for forbidden in [
        "nsec1-secret",
        "secret-auth",
        "secret system prompt",
        "gpt-secret-model",
        "persona-",
        "channel-secret",
        "env-secret",
        "private_key_nsec",
        "auth_tag",
        "system_prompt",
        "provider",
        "model",
        "persona_id",
        "team_id",
        "channel_ids",
        "ARBITRARY_SECRET",
        "updated_at",
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
fn wrong_store_hash_and_live_expected_pid_leave_store_unchanged() {
    let hash_fixture = Fixture::new();
    let hash_before = hash_fixture.store_bytes();
    let mut hash_request = hash_fixture.request();
    hash_request["expectedStoreSha256"] = json!("0".repeat(64));
    assert_eq!(
        error_code(hash_fixture.execute(&hash_request, false, &CountingInspector::default())),
        "store_hash_mismatch"
    );
    assert_eq!(hash_fixture.store_bytes(), hash_before);

    let pid_fixture = Fixture::new();
    let pid_before = pid_fixture.store_bytes();
    let mut pid_request = pid_fixture.request();
    pid_request["expectedDesktopPid"] = json!(std::process::id());
    assert_eq!(
        error_code(pid_fixture.execute(&pid_request, false, &CountingInspector::default())),
        "desktop_pid_alive"
    );
    assert_eq!(pid_fixture.store_bytes(), pid_before);
}
