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

    fn model_effort_request(&self) -> Value {
        let targets: Vec<Value> = CANONICAL_AGENT_EFFORTS
            .iter()
            .zip(&self.pubkeys)
            .map(|((name, effort_level), pubkey)| {
                json!({
                    "name": name,
                    "pubkey": pubkey,
                    "effortLevel": effort_level
                })
            })
            .collect();
        json!({
            "schemaVersion": MODEL_EFFORT_SCHEMA_VERSION,
            "expectedStoreSha256": sha256(&self.store_bytes()),
            "expectedAgentCount": CANONICAL_AGENT_COUNT,
            "expectedDesktopPid": dead_pid(),
            "targets": targets,
            "desiredModel": CANONICAL_MODEL
        })
    }

    fn prepare_model_effort_store(&self) {
        write_store(&self.store, &model_effort_records(&self.pubkeys));
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
    acp_calls: Cell<usize>,
    fail_on_call: Option<usize>,
    acp_fail_on_call: Option<usize>,
    mutate_on_call: Option<(usize, PathBuf, Vec<u8>, u32)>,
}

impl CountingInspector {
    fn fail_on(call: usize) -> Self {
        Self {
            calls: Cell::new(0),
            acp_calls: Cell::new(0),
            fail_on_call: Some(call),
            acp_fail_on_call: None,
            mutate_on_call: None,
        }
    }

    fn fail_on_acp(call: usize) -> Self {
        Self {
            calls: Cell::new(0),
            acp_calls: Cell::new(0),
            fail_on_call: None,
            acp_fail_on_call: Some(call),
            mutate_on_call: None,
        }
    }

    fn mutate_on(call: usize, path: PathBuf, bytes: Vec<u8>, mode: u32) -> Self {
        Self {
            calls: Cell::new(0),
            acp_calls: Cell::new(0),
            fail_on_call: None,
            acp_fail_on_call: None,
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

    fn ensure_acp_absent(&self) -> Result<(), ControlError> {
        let call = self.acp_calls.get() + 1;
        self.acp_calls.set(call);
        if self.acp_fail_on_call == Some(call) {
            return Err(ControlError::new(
                "acp_process_alive",
                "test buzz-acp worker survived at final fence",
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

fn model_effort_records(pubkeys: &[String]) -> Value {
    let base = base_records(pubkeys);
    let base_records = base.as_array().expect("base records");
    let mut definitions = Vec::with_capacity(base_records.len());
    let mut instances = Vec::with_capacity(CANONICAL_AGENT_COUNT);
    for (index, ((name, _), pubkey)) in CANONICAL_AGENT_EFFORTS.iter().zip(pubkeys).enumerate() {
        let slug = format!("persona-{index}");
        let mut definition = base_records[index * 2 + 1].clone();
        definition["name"] = json!(name);
        definition["display_name"] = json!(name);
        definition["slug"] = json!(slug);
        definition["persona_id"] = Value::Null;
        definition["model"] = json!(format!("gpt-secret-definition-model-{index}"));
        definitions.push(definition);

        let mut instance = base_records[index * 2].clone();
        instance["name"] = json!(name);
        instance["pubkey"] = json!(pubkey);
        instance["persona_id"] = json!(slug);
        instance["effort_level"] = json!(format!("opaque-old-effort-{index}"));
        instances.push(instance);
    }
    definitions.extend(instances);
    definitions.extend(
        base_records[CANONICAL_AGENT_COUNT * 2..]
            .iter()
            .cloned(),
    );
    Value::Array(definitions)
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
            "BUZZ_ACP_TURN_SEGMENT",
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
fn production_forward_contract_is_exactly_the_approved_immutable_release() {
    let forward = production_forward_artifacts();
    assert_eq!(
        forward.release_id,
        "1e2eabb806324b4bbc1403d58633b8760a36f015"
    );
    assert_eq!(
        forward.source_tree,
        "1c7210e9ea80e48d0bb400b2dc363e9acb2fe6ef"
    );
    assert_eq!(
        forward.manifest_sha256,
        "adf4587aa77ed5c35e21ae406163845e0265697e3503c83809f0c045a2925b82"
    );
    assert_eq!(
        forward.command_sha256,
        "8d2720ddde69d25a0d21c28bdd1308cf524243d8cdb86781965a7ade98858745"
    );
    assert_eq!(forward.command_size, 184);
    assert_eq!(
        forward.libexec_sha256,
        "0814252a3ff57cdc8b5116d478a2540609cbc28925e8b95bae7cd51d29a62492"
    );
    assert_eq!(forward.libexec_size, 13_915_904);
    assert_eq!(forward.owner, "timi");
    assert_eq!(forward.mode, "0555");
    assert_eq!(forward.toolchain, "rustc 1.95.0");
    assert!(matches!(
        forward.mcp,
        McpContract::RuntimeArtifact {
            sha256: "6e67dc3b8aa1d78a2907ec55400641113ded170e3eb7d7a84f2dc8bd95935b01",
            size: 20_077_248
        }
    ));
    assert_eq!(
        forward.environment.env_set(),
        BTreeMap::from([
            ("BUZZ_ACP_HEARTBEAT_INTERVAL".to_owned(), "900".to_owned()),
            ("BUZZ_ACP_HEARTBEAT_MODE".to_owned(), "schedules".to_owned()),
            ("BUZZ_ACP_LAZY_POOL".to_owned(), "true".to_owned()),
            ("BUZZ_ACP_IDLE_POOL_SLEEP".to_owned(), "300".to_owned()),
            ("BUZZ_ACP_TURN_SEGMENT".to_owned(), "2700".to_owned())
        ])
    );
    assert!(forward.environment.env_unset().is_empty());
}

#[test]
fn production_inverse_contract_is_exactly_the_approved_immutable_release() {
    let inverse = production_rollback_artifacts();
    assert_eq!(
        inverse.release_id,
        "c21731e00b4540599cbec138615ee18083874bdb"
    );
    assert_eq!(
        inverse.source_tree,
        "69176cd1a21400223fe43a3e9a0e7b3fb8f8f95f"
    );
    assert_eq!(
        inverse.manifest_sha256,
        "792cc5b2d2954c7c97a1ed009bc6c84e96bafa88c4d4d34ec509db070aa33760"
    );
    assert_eq!(
        inverse.command_sha256,
        "8d2720ddde69d25a0d21c28bdd1308cf524243d8cdb86781965a7ade98858745"
    );
    assert_eq!(inverse.command_size, 184);
    assert_eq!(
        inverse.libexec_sha256,
        "fafa196e27475fcd5c36d1f44105068d97ff587c69945b0d5e6c31b2ec3a297c"
    );
    assert_eq!(inverse.libexec_size, 13_915_904);
    assert_eq!(inverse.owner, "timi");
    assert_eq!(inverse.mode, "0555");
    assert_eq!(inverse.toolchain, "rustc 1.95.0");
    assert!(matches!(
        inverse.mcp,
        McpContract::RuntimeArtifact {
            sha256: "f4a96c0a0236a5618ce3e3bbf377e26ee370b516727cab1cf54727be8a529f9b",
            size: 20_076_608
        }
    ));
    assert_eq!(
        inverse.environment.env_set(),
        BTreeMap::from([
            ("BUZZ_ACP_HEARTBEAT_INTERVAL".to_owned(), "900".to_owned()),
            ("BUZZ_ACP_HEARTBEAT_MODE".to_owned(), "schedules".to_owned()),
            ("BUZZ_ACP_LAZY_POOL".to_owned(), "true".to_owned()),
            ("BUZZ_ACP_IDLE_POOL_SLEEP".to_owned(), "300".to_owned()),
            ("BUZZ_ACP_TURN_SEGMENT".to_owned(), "2700".to_owned())
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
            "BUZZ_ACP_TURN_SEGMENT",
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

#[test]
fn schema_v1_receipt_and_model_effort_behavior_remain_unchanged() {
    let fixture = Fixture::new();
    let before = parse_store(&fixture);
    let before_sha256 = sha256(&fixture.store_bytes());
    let receipt = fixture
        .execute(&fixture.request(), false, &CountingInspector::default())
        .expect("apply schema v1 patch");
    let after = parse_store(&fixture);
    for (before_record, after_record) in before
        .as_array()
        .expect("before records")
        .iter()
        .zip(after.as_array().expect("after records"))
    {
        assert_eq!(before_record.get("model"), after_record.get("model"));
        assert_eq!(
            before_record.get("effort_level"),
            after_record.get("effort_level")
        );
    }
    let serialized = serde_json::to_value(&receipt).expect("serialize v1 receipt");
    assert_eq!(
        serialized,
        json!({
            "schemaVersion": 1,
            "status": "applied",
            "expectedStoreSha256": before_sha256,
            "actualBeforeSha256": before_sha256,
            "afterSha256": sha256(&fixture.store_bytes()),
            "storePath": fixture.store.to_string_lossy(),
            "targetPubkeys": fixture.pubkeys,
            "changedFields": ["acp_command", "env_vars", "mcp_command"],
            "changedEnvKeys": [
                "BUZZ_ACP_HEARTBEAT_INTERVAL",
                "BUZZ_ACP_HEARTBEAT_MODE",
                "BUZZ_ACP_IDLE_POOL_SLEEP",
                "BUZZ_ACP_LAZY_POOL"
            ],
            "parallelism": 10,
            "acpCommand": fixture.wrapper.to_string_lossy(),
            "mcpCommand": fixture.mcp.to_string_lossy(),
            "agentCount": 9,
            "releaseId": RELEASE_ID,
            "sourceTree": SOURCE_TREE,
            "manifestSha256": fixture.manifest_hash,
            "acpCommandSha256": fixture.wrapper_hash,
            "libexecSha256": fixture.libexec_hash,
            "mcpCommandSha256": fixture.mcp_hash
        })
    );
}

#[test]
fn schema_v2_applies_exact_model_effort_profile_and_preserves_opaque_data() {
    let fixture = Fixture::new();
    fixture.prepare_model_effort_store();
    let before = parse_store(&fixture);
    let inspector = CountingInspector::default();
    let receipt = fixture
        .execute(&fixture.model_effort_request(), false, &inspector)
        .expect("apply schema v2 model/effort reset");
    let after = parse_store(&fixture);
    assert_eq!(inspector.calls.get(), 2, "both Desktop fences must run");
    assert_eq!(inspector.acp_calls.get(), 2, "both ACP fences must run");
    assert_eq!(
        after.as_array().expect("after records").len(),
        before.as_array().expect("before records").len()
    );

    let target_persona_ids: HashSet<&str> = before
        .as_array()
        .expect("before records")
        .iter()
        .filter(|record| record["pubkey"] != "")
        .map(|record| {
            record["persona_id"]
                .as_str()
                .expect("target persona id")
        })
        .collect();
    let mut linked_source_slugs = HashSet::new();

    for (before_record, after_record) in before
        .as_array()
        .expect("before records")
        .iter()
        .zip(after.as_array().expect("after records"))
    {
        if before_record["pubkey"] == "" {
            let is_target_linked = before_record
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| target_persona_ids.contains(slug));
            if !is_target_linked {
                assert_eq!(
                    after_record, before_record,
                    "unrelated keyless definition changed"
                );
                continue;
            }
            assert_eq!(after_record["model"], CANONICAL_MODEL);
            assert_ne!(before_record["model"], after_record["model"]);
            let mut protected_before = before_record.as_object().expect("before object").clone();
            let mut protected_after = after_record.as_object().expect("after object").clone();
            protected_before.remove("model");
            protected_after.remove("model");
            assert_eq!(
                protected_after, protected_before,
                "source definition opaque field changed"
            );
            continue;
        }
        let name = before_record["name"].as_str().expect("agent name");
        let persona_id = before_record["persona_id"]
            .as_str()
            .expect("linked persona id");
        assert!(
            linked_source_slugs.insert(persona_id),
            "source definition linked more than once"
        );
        let source = after
            .as_array()
            .expect("after records")
            .iter()
            .find(|record| record["pubkey"] == "" && record["slug"] == persona_id)
            .expect("linked source definition");
        assert_eq!(source["model"], CANONICAL_MODEL);
        let expected_effort = CANONICAL_AGENT_EFFORTS
            .iter()
            .find_map(|(canonical_name, effort)| (*canonical_name == name).then_some(*effort))
            .expect("canonical agent name");
        assert_eq!(after_record["model"], CANONICAL_MODEL);
        assert_eq!(after_record["effort_level"], expected_effort);
        assert_ne!(before_record["model"], after_record["model"]);
        assert_ne!(before_record["effort_level"], after_record["effort_level"]);

        let mut protected_before = before_record.as_object().expect("before object").clone();
        let mut protected_after = after_record.as_object().expect("after object").clone();
        for field in ["model", "effort_level"] {
            protected_before.remove(field);
            protected_after.remove(field);
        }
        assert_eq!(protected_after, protected_before, "opaque field changed");
    }
    assert_eq!(linked_source_slugs.len(), CANONICAL_AGENT_COUNT);

    assert_eq!(
        receipt.changed_fields,
        vec!["effort_level".to_owned(), "model".to_owned()]
    );
    assert_eq!(receipt.after_sha256, sha256(&fixture.store_bytes()));
    let serialized = serde_json::to_value(&receipt).expect("serialize v2 receipt");
    let keys: BTreeSet<&str> = serialized
        .as_object()
        .expect("v2 receipt object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "schemaVersion",
            "status",
            "actualBeforeSha256",
            "afterSha256",
            "agentCount",
            "changedFields",
            "desiredModel",
            "mediumCount",
            "highCount",
        ])
    );
    assert_eq!(serialized["schemaVersion"], MODEL_EFFORT_SCHEMA_VERSION);
    assert_eq!(serialized["agentCount"], CANONICAL_AGENT_COUNT);
    assert_eq!(serialized["desiredModel"], CANONICAL_MODEL);
    assert_eq!(serialized["mediumCount"], 8);
    assert_eq!(serialized["highCount"], 1);
    let output = serde_json::to_string(&receipt).expect("serialize v2 output");
    for forbidden in fixture.pubkeys.iter().map(String::as_str).chain([
        "nsec1-secret",
        "secret-auth",
        "secret system prompt",
        "ARBITRARY_SECRET",
        "private_key_nsec",
        "system_prompt",
        "buzz-acp",
        "buzz-dev-mcp",
        "PM Bot",
        "Koder",
    ]) {
        assert!(!output.contains(forbidden), "v2 receipt leaked {forbidden}");
    }
    for forbidden_path in [&fixture.store, &fixture.wrapper, &fixture.mcp] {
        assert!(
            !output.contains(forbidden_path.to_string_lossy().as_ref()),
            "v2 receipt leaked a path"
        );
    }
}

#[test]
fn schema_v2_rejects_wrong_model_and_effort_without_mutation() {
    let model_fixture = Fixture::new();
    model_fixture.prepare_model_effort_store();
    let model_before = model_fixture.store_bytes();
    let mut wrong_model = model_fixture.model_effort_request();
    wrong_model["desiredModel"] = json!("gpt-5.6-sol");
    assert_eq!(
        error_code(model_fixture.execute(&wrong_model, false, &CountingInspector::default())),
        "invalid_desired_model"
    );
    assert_eq!(model_fixture.store_bytes(), model_before);

    let effort_fixture = Fixture::new();
    effort_fixture.prepare_model_effort_store();
    let effort_before = effort_fixture.store_bytes();
    let mut wrong_effort = effort_fixture.model_effort_request();
    wrong_effort["targets"][0]["effortLevel"] = json!("high");
    assert_eq!(
        error_code(effort_fixture.execute(&wrong_effort, false, &CountingInspector::default())),
        "invalid_target_effort"
    );
    assert_eq!(effort_fixture.store_bytes(), effort_before);
}

#[test]
fn schema_v2_rejects_wrong_name_binding_missing_and_extra_targets() {
    let mapping_fixture = Fixture::new();
    mapping_fixture.prepare_model_effort_store();
    let mapping_before = mapping_fixture.store_bytes();
    let mut wrong_mapping = mapping_fixture.model_effort_request();
    let first_pubkey = wrong_mapping["targets"][0]["pubkey"].clone();
    wrong_mapping["targets"][0]["pubkey"] = wrong_mapping["targets"][1]["pubkey"].clone();
    wrong_mapping["targets"][1]["pubkey"] = first_pubkey;
    assert_eq!(
        error_code(mapping_fixture.execute(&wrong_mapping, false, &CountingInspector::default())),
        "target_name_mismatch"
    );
    assert_eq!(mapping_fixture.store_bytes(), mapping_before);

    let stored_name_fixture = Fixture::new();
    stored_name_fixture.prepare_model_effort_store();
    let mut wrong_stored_name = parse_store(&stored_name_fixture);
    wrong_stored_name[CANONICAL_AGENT_COUNT]["name"] = json!("PM Bot renamed");
    write_store(&stored_name_fixture.store, &wrong_stored_name);
    let stored_name_before = stored_name_fixture.store_bytes();
    let stored_name_request = stored_name_fixture.model_effort_request();
    assert_eq!(
        error_code(stored_name_fixture.execute(
            &stored_name_request,
            false,
            &CountingInspector::default()
        )),
        "target_name_mismatch"
    );
    assert_eq!(stored_name_fixture.store_bytes(), stored_name_before);

    let missing_fixture = Fixture::new();
    missing_fixture.prepare_model_effort_store();
    let missing_before = missing_fixture.store_bytes();
    let mut missing = missing_fixture.model_effort_request();
    missing["targets"]
        .as_array_mut()
        .expect("targets array")
        .pop();
    assert_eq!(
        error_code(missing_fixture.execute(&missing, false, &CountingInspector::default())),
        "invalid_target_count"
    );
    assert_eq!(missing_fixture.store_bytes(), missing_before);

    let extra_fixture = Fixture::new();
    extra_fixture.prepare_model_effort_store();
    let extra_before = extra_fixture.store_bytes();
    let mut extra = extra_fixture.model_effort_request();
    let extra_target = extra["targets"][0].clone();
    extra["targets"]
        .as_array_mut()
        .expect("targets array")
        .push(extra_target);
    assert_eq!(
        error_code(extra_fixture.execute(&extra, false, &CountingInspector::default())),
        "invalid_target_count"
    );
    assert_eq!(extra_fixture.store_bytes(), extra_before);
}

#[test]
fn schema_v2_rejects_invalid_source_definition_mappings_without_mutation() {
    let absent_fixture = Fixture::new();
    absent_fixture.prepare_model_effort_store();
    let mut absent = parse_store(&absent_fixture);
    absent.as_array_mut().expect("records").remove(0);
    write_store(&absent_fixture.store, &absent);
    let absent_before = absent_fixture.store_bytes();
    assert_eq!(
        error_code(absent_fixture.execute(
            &absent_fixture.model_effort_request(),
            false,
            &CountingInspector::default()
        )),
        "source_definition_mismatch"
    );
    assert_eq!(absent_fixture.store_bytes(), absent_before);

    let duplicate_fixture = Fixture::new();
    duplicate_fixture.prepare_model_effort_store();
    let mut duplicate = parse_store(&duplicate_fixture);
    duplicate[1]["slug"] = duplicate[0]["slug"].clone();
    write_store(&duplicate_fixture.store, &duplicate);
    let duplicate_before = duplicate_fixture.store_bytes();
    assert_eq!(
        error_code(duplicate_fixture.execute(
            &duplicate_fixture.model_effort_request(),
            false,
            &CountingInspector::default()
        )),
        "duplicate_source_definition"
    );
    assert_eq!(duplicate_fixture.store_bytes(), duplicate_before);

    let mismatch_fixture = Fixture::new();
    mismatch_fixture.prepare_model_effort_store();
    let mut mismatch = parse_store(&mismatch_fixture);
    mismatch[CANONICAL_AGENT_COUNT]["persona_id"] = json!("persona-does-not-exist");
    write_store(&mismatch_fixture.store, &mismatch);
    let mismatch_before = mismatch_fixture.store_bytes();
    assert_eq!(
        error_code(mismatch_fixture.execute(
            &mismatch_fixture.model_effort_request(),
            false,
            &CountingInspector::default()
        )),
        "source_definition_mismatch"
    );
    assert_eq!(mismatch_fixture.store_bytes(), mismatch_before);

    let extra_fixture = Fixture::new();
    extra_fixture.prepare_model_effort_store();
    let mut extra = parse_store(&extra_fixture);
    let extra_definition = json!({
        "pubkey": "",
        "slug": "persona-unrelated",
        "name": "Unrelated definition",
        "model": "gpt-unrelated-model",
        "opaque": {"preserve": [3, 2, 1]}
    });
    let extra_index = extra.as_array().expect("records").len();
    extra
        .as_array_mut()
        .expect("records")
        .push(extra_definition.clone());
    write_store(&extra_fixture.store, &extra);
    extra_fixture
        .execute(
            &extra_fixture.model_effort_request(),
            false,
            &CountingInspector::default()
        )
        .expect("unrelated definition must not block schema v2");
    let extra_after = parse_store(&extra_fixture);
    assert_eq!(extra_after[extra_index], extra_definition);
}

#[test]
fn schema_v2_rejects_malformed_pubkey_count_and_hash_without_mutation() {
    let pubkey_fixture = Fixture::new();
    pubkey_fixture.prepare_model_effort_store();
    let pubkey_before = pubkey_fixture.store_bytes();
    let mut wrong_pubkey = pubkey_fixture.model_effort_request();
    wrong_pubkey["targets"][0]["pubkey"] = json!("f".repeat(64));
    assert_eq!(
        error_code(pubkey_fixture.execute(&wrong_pubkey, false, &CountingInspector::default())),
        "target_set_mismatch"
    );
    assert_eq!(pubkey_fixture.store_bytes(), pubkey_before);

    let malformed_fixture = Fixture::new();
    malformed_fixture.prepare_model_effort_store();
    let malformed_before = malformed_fixture.store_bytes();
    let mut malformed = malformed_fixture.model_effort_request();
    malformed["targets"][0]
        .as_object_mut()
        .expect("target object")
        .remove("name");
    assert_eq!(
        error_code(malformed_fixture.execute(&malformed, false, &CountingInspector::default())),
        "invalid_request_json"
    );
    assert_eq!(malformed_fixture.store_bytes(), malformed_before);

    let count_fixture = Fixture::new();
    count_fixture.prepare_model_effort_store();
    let count_before = count_fixture.store_bytes();
    let mut wrong_count = count_fixture.model_effort_request();
    wrong_count["expectedAgentCount"] = json!(8);
    assert_eq!(
        error_code(count_fixture.execute(&wrong_count, false, &CountingInspector::default())),
        "invalid_expected_agent_count"
    );
    assert_eq!(count_fixture.store_bytes(), count_before);

    let hash_fixture = Fixture::new();
    hash_fixture.prepare_model_effort_store();
    let hash_before = hash_fixture.store_bytes();
    let mut wrong_hash = hash_fixture.model_effort_request();
    wrong_hash["expectedStoreSha256"] = json!("0".repeat(64));
    assert_eq!(
        error_code(hash_fixture.execute(&wrong_hash, false, &CountingInspector::default())),
        "store_hash_mismatch"
    );
    assert_eq!(hash_fixture.store_bytes(), hash_before);
}

#[test]
fn schema_v2_final_desktop_and_store_races_do_not_commit_candidate() {
    let desktop_fixture = Fixture::new();
    desktop_fixture.prepare_model_effort_store();
    let desktop_before = desktop_fixture.store_bytes();
    let desktop_request = desktop_fixture.model_effort_request();
    let desktop_inspector = CountingInspector::fail_on(2);
    assert_eq!(
        error_code(desktop_fixture.execute(&desktop_request, false, &desktop_inspector)),
        "desktop_process_alive"
    );
    assert_eq!(desktop_inspector.calls.get(), 2);
    assert_eq!(desktop_inspector.acp_calls.get(), 1);
    assert_eq!(desktop_fixture.store_bytes(), desktop_before);

    let race_fixture = Fixture::new();
    race_fixture.prepare_model_effort_store();
    let race_request = race_fixture.model_effort_request();
    let mut raced_store = parse_store(&race_fixture);
    raced_store[0]["unknown_extension"]["racing_writer"] = json!(true);
    let mut raced_bytes = serde_json::to_vec_pretty(&raced_store).expect("serialize raced store");
    raced_bytes.push(b'\n');
    let race_inspector =
        CountingInspector::mutate_on(2, race_fixture.store.clone(), raced_bytes.clone(), 0o600);
    assert_eq!(
        error_code(race_fixture.execute(&race_request, false, &race_inspector)),
        "store_changed_before_commit"
    );
    assert_eq!(race_inspector.calls.get(), 2);
    assert_eq!(race_inspector.acp_calls.get(), 2);
    assert_eq!(
        race_fixture.store_bytes(),
        raced_bytes,
        "operator overwrote the racing store write"
    );
}

#[test]
fn schema_v2_final_acp_worker_fence_failure_leaves_store_unchanged() {
    let fixture = Fixture::new();
    fixture.prepare_model_effort_store();
    let before = fixture.store_bytes();
    let request = fixture.model_effort_request();
    let inspector = CountingInspector::fail_on_acp(2);

    assert_eq!(
        error_code(fixture.execute(&request, false, &inspector)),
        "acp_process_alive"
    );
    assert_eq!(inspector.calls.get(), 2, "both process fences must run");
    assert_eq!(inspector.acp_calls.get(), 2, "both ACP fences must run");
    assert_eq!(fixture.store_bytes(), before);
}

#[test]
fn schema_v2_semantic_diff_rejects_any_opaque_change() {
    let pubkeys = test_pubkeys();
    let original = model_effort_records(&pubkeys);
    let mut candidate = original.clone();
    candidate[0]["unknown_definition_field"] = json!("changed-opaque-value");
    let target_indices: Vec<usize> =
        (CANONICAL_AGENT_COUNT..CANONICAL_AGENT_COUNT * 2).collect();
    let source_indices: Vec<usize> = (0..CANONICAL_AGENT_COUNT).collect();
    assert_eq!(
        validate_model_effort_diff(&original, &candidate, &target_indices, &source_indices)
            .expect_err("opaque change must fail")
            .code,
        "candidate_diff_rejected"
    );
}
