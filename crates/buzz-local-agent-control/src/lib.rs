//! A deliberately narrow offline control surface for Buzz-managed agents.
//!
//! The binary edits the exact production `managed-agents.json` only while the
//! caller's expected Desktop PID and independent Desktop process scans prove
//! Buzz is stopped. Opaque records stay as JSON values so credentials, prompts,
//! and arbitrary environment values never enter typed output fields.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const REQUIRED_PARALLELISM: u32 = 10;
const PRODUCTION_RUNTIME_ROOT: &str = "/Users/timi/.buzz/RUNTIMES/buzz-heartbeat";
const CANONICAL_STORE_PATH: &str =
    "/Users/timi/Library/Application Support/xyz.block.buzz.app/agents/managed-agents.json";
const DESKTOP_EXECUTABLE_PATH: &str = "/Applications/Buzz.app/Contents/MacOS/buzz-desktop";
const STORE_FILENAME: &str = "managed-agents.json";
const FORWARD_RELEASE_ID: &str = "f43207297fdeeed6919563544b970930f9f1bdb1";
const FORWARD_SOURCE_TREE: &str = "b3f4a4de4085e3fc8b74e8dcbb26919f26c1ee34";
const FORWARD_MANIFEST_SHA256: &str =
    "32bfa32988249a08b59b761a6b2c6fa67995b828602689e495aaa84f2f7b186a";
const FORWARD_COMMAND_SHA256: &str =
    "8d2720ddde69d25a0d21c28bdd1308cf524243d8cdb86781965a7ade98858745";
const FORWARD_COMMAND_SIZE: u64 = 184;
const FORWARD_LIBEXEC_SHA256: &str =
    "02852e83f379cd60b9f8f41e9708da46b10e828f35a364480defe8fd151ddd52";
const FORWARD_LIBEXEC_SIZE: u64 = 13_905_904;
const FORWARD_MCP_SHA256: &str = "9862b564e966a8a65d5f8fe5a376dd81bec8f09b81f0291897b0d6eb36ca684f";
const FORWARD_MCP_SIZE: u64 = 20_094_048;
const FORWARD_TOOLCHAIN: &str = "rustc 1.95.0";
const ROLLBACK_RELEASE_ID: &str = "9dfba9c4e2a1d26ff7041527b739ffaebd045152";
const ROLLBACK_SOURCE_TREE: &str = "b7c84b3a6ec88ff83a71324fcda6c80228063284";
const ROLLBACK_MANIFEST_SHA256: &str =
    "5f7c777d87ab0f978e8e1ed1ff5b537008f63168c23051fc9e49f8820f9705f0";
const ROLLBACK_COMMAND_SHA256: &str =
    "8d2720ddde69d25a0d21c28bdd1308cf524243d8cdb86781965a7ade98858745";
const ROLLBACK_COMMAND_SIZE: u64 = 184;
const ROLLBACK_LIBEXEC_SHA256: &str =
    "496b06f184f5938744c11402a143538200aa3f164359423f9f62e39a0f2c32d6";
const ROLLBACK_LIBEXEC_SIZE: u64 = 13_905_904;
const ROLLBACK_MCP_SHA256: &str =
    "b7f30921375743c4f1af5db51b828e00c9734e5350832994d1680f900dc2abeb";
const ROLLBACK_MCP_SIZE: u64 = 20_023_584;
const ROLLBACK_TOOLCHAIN: &str = "rustc 1.95.0";
const APPROVED_ARTIFACT_OWNER: &str = "timi";
const APPROVED_ARTIFACT_MODE: &str = "0555";
const CANONICAL_AGENT_COUNT: usize = 9;
const ALLOWED_ENV_KEYS: [&str; 4] = [
    "BUZZ_ACP_HEARTBEAT_INTERVAL",
    "BUZZ_ACP_HEARTBEAT_MODE",
    "BUZZ_ACP_LAZY_POOL",
    "BUZZ_ACP_IDLE_POOL_SLEEP",
];

/// Command-line inputs after argument parsing.
#[derive(Debug, Clone)]
pub struct CliOptions {
    /// JSON request path.
    pub request_path: PathBuf,
    /// Exact absolute managed-agent store path.
    pub store_path: PathBuf,
    /// Validate and report without writing.
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlRequest {
    schema_version: u32,
    expected_store_sha256: String,
    expected_agent_count: usize,
    #[serde(default)]
    expected_desktop_pid: Option<u32>,
    target_pubkeys: Vec<String>,
    acp_command: String,
    mcp_command: String,
    expected_release_id: String,
    expected_source_tree: String,
    expected_manifest_sha256: String,
    expected_acp_command_sha256: String,
    expected_acp_command_size: u64,
    expected_libexec_sha256: String,
    expected_libexec_size: u64,
    expected_mcp_command_sha256: String,
    expected_mcp_command_size: u64,
    expected_artifact_owner: String,
    expected_artifact_mode: String,
    parallelism: u32,
    #[serde(default)]
    env_set: BTreeMap<String, String>,
    #[serde(default)]
    env_unset: Vec<String>,
}

/// Secret-free success or dry-run receipt emitted only on stdout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    schema_version: u32,
    status: String,
    expected_store_sha256: String,
    actual_before_sha256: String,
    after_sha256: String,
    store_path: String,
    target_pubkeys: Vec<String>,
    changed_fields: Vec<String>,
    changed_env_keys: Vec<String>,
    parallelism: u32,
    acp_command: String,
    mcp_command: String,
    agent_count: usize,
    release_id: String,
    source_tree: String,
    manifest_sha256: String,
    acp_command_sha256: String,
    libexec_sha256: String,
    mcp_command_sha256: String,
}

/// Secret-free structured failure written to stderr.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorReceipt {
    schema_version: u32,
    status: &'static str,
    code: &'static str,
    message: &'static str,
}

impl ErrorReceipt {
    /// Structured replacement for clap's ordinary free-form argument error.
    pub fn invalid_cli() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: "error",
            code: "invalid_cli_arguments",
            message: "required arguments are --request <FILE> and --store <managed-agents.json>",
        }
    }

    /// Last-resort error used only if a success receipt cannot be serialized.
    pub fn internal_serialization() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: "error",
            code: "internal_serialization",
            message: "failed to serialize structured output",
        }
    }
}

/// An execution failure with a deliberately redacted public representation.
#[derive(Debug)]
pub struct ControlError {
    code: &'static str,
    message: &'static str,
}

impl ControlError {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Convert the failure into the only representation permitted on stderr.
    pub fn receipt(&self) -> ErrorReceipt {
        ErrorReceipt {
            schema_version: SCHEMA_VERSION,
            status: "error",
            code: self.code,
            message: self.message,
        }
    }
}

struct ExecutionContext<'a> {
    runtime_root: &'a Path,
    canonical_store_path: &'a Path,
    desktop_executable: &'a Path,
    expected_agent_count: usize,
    forward_artifacts: ArtifactContract<'a>,
    rollback_artifacts: ArtifactContract<'a>,
    process_inspector: &'a dyn ProcessInspector,
}

#[derive(Clone, Copy)]
struct ArtifactContract<'a> {
    release_id: &'a str,
    source_tree: &'a str,
    manifest_sha256: &'a str,
    command_sha256: &'a str,
    command_size: u64,
    libexec_sha256: &'a str,
    libexec_size: u64,
    mcp: McpContract<'a>,
    owner: &'a str,
    mode: &'a str,
    toolchain: &'a str,
    environment: EnvironmentContract,
}

#[derive(Clone, Copy)]
enum McpContract<'a> {
    RuntimeArtifact {
        sha256: &'a str,
        size: u64,
    },
    // Kept for the fail-closed regression fixtures that exercise older
    // approved contracts without a release-local MCP binary.
    #[allow(dead_code)]
    BundledCommand,
}

#[derive(Clone, Copy)]
enum EnvironmentContract {
    Forward,
    Rollback,
}

fn production_forward_artifacts() -> ArtifactContract<'static> {
    ArtifactContract {
        release_id: FORWARD_RELEASE_ID,
        source_tree: FORWARD_SOURCE_TREE,
        manifest_sha256: FORWARD_MANIFEST_SHA256,
        command_sha256: FORWARD_COMMAND_SHA256,
        command_size: FORWARD_COMMAND_SIZE,
        libexec_sha256: FORWARD_LIBEXEC_SHA256,
        libexec_size: FORWARD_LIBEXEC_SIZE,
        mcp: McpContract::RuntimeArtifact {
            sha256: FORWARD_MCP_SHA256,
            size: FORWARD_MCP_SIZE,
        },
        owner: APPROVED_ARTIFACT_OWNER,
        mode: APPROVED_ARTIFACT_MODE,
        toolchain: FORWARD_TOOLCHAIN,
        environment: EnvironmentContract::Forward,
    }
}

fn production_rollback_artifacts() -> ArtifactContract<'static> {
    ArtifactContract {
        release_id: ROLLBACK_RELEASE_ID,
        source_tree: ROLLBACK_SOURCE_TREE,
        manifest_sha256: ROLLBACK_MANIFEST_SHA256,
        command_sha256: ROLLBACK_COMMAND_SHA256,
        command_size: ROLLBACK_COMMAND_SIZE,
        libexec_sha256: ROLLBACK_LIBEXEC_SHA256,
        libexec_size: ROLLBACK_LIBEXEC_SIZE,
        mcp: McpContract::RuntimeArtifact {
            sha256: ROLLBACK_MCP_SHA256,
            size: ROLLBACK_MCP_SIZE,
        },
        owner: APPROVED_ARTIFACT_OWNER,
        mode: APPROVED_ARTIFACT_MODE,
        toolchain: ROLLBACK_TOOLCHAIN,
        environment: EnvironmentContract::Rollback,
    }
}

trait ProcessInspector {
    fn ensure_desktop_absent(&self, executable: &Path) -> Result<(), ControlError>;
}

struct SystemProcessInspector;

impl ProcessInspector for SystemProcessInspector {
    fn ensure_desktop_absent(&self, executable: &Path) -> Result<(), ControlError> {
        if executable != Path::new(DESKTOP_EXECUTABLE_PATH) {
            return Err(ControlError::new(
                "invalid_desktop_executable",
                "desktop process fence requires the canonical Buzz executable path",
            ));
        }
        require_exact_regular_file(
            executable,
            "invalid_desktop_executable",
            "canonical Buzz Desktop executable must be a non-symlink regular file",
        )?;
        if pgrep_has_match(&["-x", "buzz-desktop"])? {
            return Err(ControlError::new(
                "desktop_process_alive",
                "a Buzz Desktop executable process is still alive",
            ));
        }
        if pgrep_has_match(&[
            "-f",
            "^/Applications/Buzz[.]app/Contents/MacOS/buzz-desktop([[:space:]]|$)",
        ])? {
            return Err(ControlError::new(
                "desktop_process_alive",
                "the canonical Buzz Desktop executable path is still live",
            ));
        }
        Ok(())
    }
}

fn pgrep_has_match(arguments: &[&str]) -> Result<bool, ControlError> {
    let output = Command::new("/usr/bin/pgrep")
        .args(arguments)
        .output()
        .map_err(|_| {
            ControlError::new(
                "desktop_process_scan_failed",
                "failed to run the bounded Buzz Desktop process scan",
            )
        })?;
    match output.status.code() {
        Some(0) => {
            let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
                ControlError::new(
                    "desktop_process_scan_failed",
                    "Buzz Desktop process scan returned invalid output",
                )
            })?;
            if output.stderr.is_empty()
                && !stdout.is_empty()
                && stdout
                    .lines()
                    .all(|line| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
            {
                Ok(true)
            } else {
                Err(ControlError::new(
                    "desktop_process_scan_failed",
                    "Buzz Desktop process scan returned malformed output",
                ))
            }
        }
        Some(1) if output.stdout.is_empty() && output.stderr.is_empty() => Ok(false),
        _ => Err(ControlError::new(
            "desktop_process_scan_failed",
            "Buzz Desktop process scan did not complete cleanly",
        )),
    }
}

struct SecureStore {
    bytes: Vec<u8>,
    identity: FileIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

struct Candidate {
    bytes: Vec<u8>,
    changed_fields: Vec<String>,
    changed_env_keys: Vec<String>,
    agent_count: usize,
}

/// Validate and, unless `dry_run` is set, atomically apply one bounded patch.
pub fn execute(options: CliOptions) -> Result<Receipt, ControlError> {
    let inspector = SystemProcessInspector;
    execute_with_context(
        options,
        &ExecutionContext {
            runtime_root: Path::new(PRODUCTION_RUNTIME_ROOT),
            canonical_store_path: Path::new(CANONICAL_STORE_PATH),
            desktop_executable: Path::new(DESKTOP_EXECUTABLE_PATH),
            expected_agent_count: CANONICAL_AGENT_COUNT,
            forward_artifacts: production_forward_artifacts(),
            rollback_artifacts: production_rollback_artifacts(),
            process_inspector: &inspector,
        },
    )
}

fn execute_with_context(
    options: CliOptions,
    context: &ExecutionContext<'_>,
) -> Result<Receipt, ControlError> {
    let request_bytes = fs::read(&options.request_path)
        .map_err(|_| ControlError::new("request_read_failed", "failed to read request file"))?;
    let request: ControlRequest = serde_json::from_slice(&request_bytes).map_err(|_| {
        ControlError::new(
            "invalid_request_json",
            "request is not valid schemaVersion 1 JSON",
        )
    })?;
    let artifacts = validate_request(&request, context)?;
    validate_release_artifacts(&request, context, artifacts)?;
    ensure_desktop_stopped(request.expected_desktop_pid, context)?;

    let store = read_secure_store(&options.store_path, context.canonical_store_path)?;
    let actual_before_sha256 = sha256(&store.bytes);
    if actual_before_sha256 != request.expected_store_sha256 {
        return Err(ControlError::new(
            "store_hash_mismatch",
            "store SHA-256 does not match expectedStoreSha256",
        ));
    }
    let candidate = build_candidate(&store.bytes, &request)?;
    let after_sha256 = sha256(&candidate.bytes);
    let receipt = Receipt {
        schema_version: SCHEMA_VERSION,
        status: if options.dry_run {
            "dry_run".to_owned()
        } else {
            "applied".to_owned()
        },
        expected_store_sha256: request.expected_store_sha256.clone(),
        actual_before_sha256,
        after_sha256,
        store_path: options.store_path.to_string_lossy().into_owned(),
        target_pubkeys: request.target_pubkeys.clone(),
        changed_fields: candidate.changed_fields,
        changed_env_keys: candidate.changed_env_keys,
        parallelism: request.parallelism,
        acp_command: request.acp_command.clone(),
        mcp_command: request.mcp_command.clone(),
        agent_count: candidate.agent_count,
        release_id: request.expected_release_id.clone(),
        source_tree: request.expected_source_tree.clone(),
        manifest_sha256: request.expected_manifest_sha256.clone(),
        acp_command_sha256: request.expected_acp_command_sha256.clone(),
        libexec_sha256: request.expected_libexec_sha256.clone(),
        mcp_command_sha256: request.expected_mcp_command_sha256.clone(),
    };
    if options.dry_run {
        return Ok(receipt);
    }

    let staged_store = stage_restricted_file(&options.store_path, &candidate.bytes)?;
    validate_release_artifacts(&request, context, artifacts)?;
    ensure_desktop_stopped(request.expected_desktop_pid, context)?;
    let current = read_secure_store(&options.store_path, context.canonical_store_path)?;
    if current.identity != store.identity || sha256(&current.bytes) != receipt.actual_before_sha256
    {
        return Err(ControlError::new(
            "store_changed_before_commit",
            "store changed after validation; no mutation was applied",
        ));
    }
    commit_staged_file(staged_store, &options.store_path, "store_commit_failed")?;
    Ok(receipt)
}

fn validate_request<'a>(
    request: &ControlRequest,
    context: &ExecutionContext<'a>,
) -> Result<ArtifactContract<'a>, ControlError> {
    if request.schema_version != SCHEMA_VERSION {
        return Err(ControlError::new(
            "unsupported_schema_version",
            "schemaVersion must equal 1",
        ));
    }
    if !is_lower_hex(&request.expected_store_sha256, 64) {
        return Err(ControlError::new(
            "invalid_expected_hash",
            "expectedStoreSha256 must be 64 lowercase hexadecimal characters",
        ));
    }
    if request.expected_agent_count != context.expected_agent_count
        || context.expected_agent_count != CANONICAL_AGENT_COUNT
    {
        return Err(ControlError::new(
            "invalid_expected_agent_count",
            "expectedAgentCount must equal the canonical fleet size of 9",
        ));
    }
    if request.target_pubkeys.len() != context.expected_agent_count {
        return Err(ControlError::new(
            "invalid_target_count",
            "targetPubkeys must contain the complete canonical fleet of 9 identities",
        ));
    }
    let mut targets = HashSet::with_capacity(request.target_pubkeys.len());
    for target in &request.target_pubkeys {
        if !is_lower_hex(target, 64) {
            return Err(ControlError::new(
                "invalid_target_pubkey",
                "every target public key must be 64 lowercase hexadecimal characters",
            ));
        }
        if !targets.insert(target.as_str()) {
            return Err(ControlError::new(
                "duplicate_target_pubkey",
                "target public keys must be unique",
            ));
        }
    }
    if request.parallelism != REQUIRED_PARALLELISM {
        return Err(ControlError::new(
            "invalid_parallelism",
            "parallelism must equal 10",
        ));
    }
    for (key, value) in &request.env_set {
        if !is_allowed_env_key(key) {
            return Err(ControlError::new(
                "invalid_env_key",
                "envSet contains a key outside the heartbeat allowlist",
            ));
        }
        if value.is_empty() {
            return Err(ControlError::new(
                "empty_env_value",
                "envSet values must be nonempty strings",
            ));
        }
    }
    let mut unset = HashSet::with_capacity(request.env_unset.len());
    for key in &request.env_unset {
        if !is_allowed_env_key(key) {
            return Err(ControlError::new(
                "invalid_env_key",
                "envUnset contains a key outside the heartbeat allowlist",
            ));
        }
        if !unset.insert(key.as_str()) {
            return Err(ControlError::new(
                "duplicate_env_unset_key",
                "envUnset keys must be unique",
            ));
        }
        if request.env_set.contains_key(key) {
            return Err(ControlError::new(
                "conflicting_env_change",
                "the same environment key cannot be set and unset",
            ));
        }
    }
    validate_requested_artifact_contract(request, context)
}

fn ensure_desktop_stopped(
    expected_pid: Option<u32>,
    context: &ExecutionContext<'_>,
) -> Result<(), ControlError> {
    require_expected_desktop_stopped(expected_pid)?;
    context
        .process_inspector
        .ensure_desktop_absent(context.desktop_executable)
}

#[cfg(unix)]
fn require_expected_desktop_stopped(pid: Option<u32>) -> Result<(), ControlError> {
    use nix::{errno::Errno, sys::signal, unistd::Pid};
    let pid = pid.ok_or_else(|| {
        ControlError::new(
            "missing_expected_desktop_pid",
            "expectedDesktopPid is required on Unix",
        )
    })?;
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            ControlError::new(
                "invalid_expected_desktop_pid",
                "expectedDesktopPid must be a positive process id",
            )
        })?;
    match signal::kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => Err(ControlError::new(
            "desktop_pid_alive",
            "expected Desktop PID is still alive; stop signed Buzz Desktop first",
        )),
        Err(Errno::ESRCH) => Ok(()),
        Err(_) => Err(ControlError::new(
            "desktop_pid_check_failed",
            "failed to prove that the expected Desktop PID is stopped",
        )),
    }
}

#[cfg(not(unix))]
fn require_expected_desktop_stopped(_pid: Option<u32>) -> Result<(), ControlError> {
    Ok(())
}

fn validate_requested_artifact_contract<'a>(
    request: &ControlRequest,
    context: &ExecutionContext<'a>,
) -> Result<ArtifactContract<'a>, ControlError> {
    for approved in [context.forward_artifacts, context.rollback_artifacts] {
        if request_matches_artifact_contract(request, context.runtime_root, approved) {
            return Ok(approved);
        }
    }
    Err(ControlError::new(
        "artifact_contract_mismatch",
        "request does not match either approved immutable heartbeat release contract",
    ))
}

fn request_matches_artifact_contract(
    request: &ControlRequest,
    runtime_root: &Path,
    approved: ArtifactContract<'_>,
) -> bool {
    let exact_command = runtime_root.join(approved.release_id).join("bin/buzz-acp");
    let (exact_mcp_command, expected_mcp_sha256, expected_mcp_size) = match approved.mcp {
        McpContract::RuntimeArtifact { sha256, size } => (
            runtime_root
                .join(approved.release_id)
                .join("bin/buzz-dev-mcp")
                .to_string_lossy()
                .into_owned(),
            sha256,
            size,
        ),
        McpContract::BundledCommand => ("buzz-dev-mcp".to_owned(), "", 0),
    };
    request.expected_release_id == approved.release_id
        && request.expected_source_tree == approved.source_tree
        && request.expected_manifest_sha256 == approved.manifest_sha256
        && request.expected_acp_command_sha256 == approved.command_sha256
        && request.expected_acp_command_size == approved.command_size
        && request.expected_libexec_sha256 == approved.libexec_sha256
        && request.expected_libexec_size == approved.libexec_size
        && request.expected_mcp_command_sha256 == expected_mcp_sha256
        && request.expected_mcp_command_size == expected_mcp_size
        && request.expected_artifact_owner == approved.owner
        && request.expected_artifact_mode == approved.mode
        && Path::new(&request.acp_command) == exact_command
        && request.mcp_command == exact_mcp_command
        && is_lower_hex(&request.expected_release_id, 40)
        && is_lower_hex(&request.expected_source_tree, 40)
        && is_lower_hex(&request.expected_manifest_sha256, 64)
        && is_lower_hex(&request.expected_acp_command_sha256, 64)
        && is_lower_hex(&request.expected_libexec_sha256, 64)
        && (matches!(approved.mcp, McpContract::BundledCommand)
            || is_lower_hex(&request.expected_mcp_command_sha256, 64))
        && request.env_set == approved.environment.env_set()
        && approved.environment.matches_env_unset(&request.env_unset)
}

impl EnvironmentContract {
    fn env_set(self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("BUZZ_ACP_HEARTBEAT_INTERVAL".to_owned(), "900".to_owned()),
            ("BUZZ_ACP_HEARTBEAT_MODE".to_owned(), "schedules".to_owned()),
            ("BUZZ_ACP_LAZY_POOL".to_owned(), "true".to_owned()),
            ("BUZZ_ACP_IDLE_POOL_SLEEP".to_owned(), "300".to_owned()),
        ])
    }

    fn env_unset(self) -> Vec<String> {
        Vec::new()
    }

    fn matches_env_unset(self, actual: &[String]) -> bool {
        let expected = self.env_unset();
        actual.len() == expected.len() && expected.iter().all(|key| actual.contains(key))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeManifest {
    schema: u32,
    source: ManifestSource,
    build: ManifestBuild,
    desktop_contract: ManifestDesktopContract,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSource {
    commit: String,
    tree: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestBuild {
    profile: String,
    target: String,
    toolchain: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDesktopContract {
    acp_command: String,
    #[serde(default)]
    mcp_command: Option<String>,
    environment: BTreeMap<String, String>,
    unchanged_desktop: String,
    unchanged_global_cli: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestArtifact {
    path: String,
    sha256: String,
    size: u64,
}

fn validate_release_artifacts(
    request: &ControlRequest,
    context: &ExecutionContext<'_>,
    approved: ArtifactContract<'_>,
) -> Result<(), ControlError> {
    let release_root = context.runtime_root.join(approved.release_id);
    require_exact_canonical_path(context.runtime_root, "runtime_root_symlink")?;
    require_exact_canonical_path(&release_root, "release_root_symlink")?;
    let owner_uid = expected_owner_uid(approved.owner)?;
    let manifest_path = release_root.join("MANIFEST.json");
    let manifest_bytes = read_restricted_artifact(
        &manifest_path,
        owner_uid,
        None,
        None,
        "manifest_validation_failed",
    )?;
    if sha256(&manifest_bytes) != request.expected_manifest_sha256 {
        return Err(ControlError::new(
            "manifest_hash_mismatch",
            "MANIFEST.json does not match the approved SHA-256",
        ));
    }
    let manifest: RuntimeManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
        ControlError::new(
            "invalid_runtime_manifest",
            "MANIFEST.json does not match the strict runtime manifest schema",
        )
    })?;
    if manifest.schema != 1
        || manifest.source.commit != request.expected_release_id
        || manifest.source.tree != request.expected_source_tree
        || manifest.build.profile != "release"
        || manifest.build.target != "aarch64-apple-darwin"
        || manifest.build.toolchain != approved.toolchain
        || manifest.desktop_contract.acp_command != request.acp_command
        || manifest.desktop_contract.mcp_command.as_deref()
            != match approved.mcp {
                McpContract::RuntimeArtifact { .. } => Some(request.mcp_command.as_str()),
                McpContract::BundledCommand => None,
            }
        || manifest.desktop_contract.environment != request.env_set
        || manifest.desktop_contract.unchanged_desktop != "/Applications/Buzz.app 0.5.19"
        || manifest.desktop_contract.unchanged_global_cli != "/Users/timi/.local/bin/buzz"
    {
        return Err(ControlError::new(
            "runtime_manifest_contract_mismatch",
            "MANIFEST.json does not declare the approved release and Desktop contract",
        ));
    }
    let mut artifacts = HashMap::with_capacity(manifest.artifacts.len());
    for artifact in &manifest.artifacts {
        if artifacts.insert(artifact.path.as_str(), artifact).is_some() {
            return Err(ControlError::new(
                "duplicate_manifest_artifact",
                "MANIFEST.json contains a duplicate artifact path",
            ));
        }
    }
    let wrapper_declaration = artifacts.get("bin/buzz-acp").ok_or_else(|| {
        ControlError::new(
            "missing_manifest_artifact",
            "MANIFEST.json does not declare bin/buzz-acp",
        )
    })?;
    let libexec_declaration = artifacts.get("libexec/buzz-acp").ok_or_else(|| {
        ControlError::new(
            "missing_manifest_artifact",
            "MANIFEST.json does not declare libexec/buzz-acp",
        )
    })?;
    let mcp_declaration = match approved.mcp {
        McpContract::RuntimeArtifact { .. } => {
            Some(artifacts.get("bin/buzz-dev-mcp").ok_or_else(|| {
                ControlError::new(
                    "missing_manifest_artifact",
                    "MANIFEST.json does not declare bin/buzz-dev-mcp",
                )
            })?)
        }
        McpContract::BundledCommand => None,
    };
    if wrapper_declaration.sha256 != request.expected_acp_command_sha256
        || wrapper_declaration.size != request.expected_acp_command_size
        || libexec_declaration.sha256 != request.expected_libexec_sha256
        || libexec_declaration.size != request.expected_libexec_size
        || mcp_declaration.is_some_and(|declaration| {
            declaration.sha256 != request.expected_mcp_command_sha256
                || declaration.size != request.expected_mcp_command_size
        })
    {
        return Err(ControlError::new(
            "runtime_manifest_artifact_mismatch",
            "MANIFEST.json artifact declarations do not match the approved request",
        ));
    }
    let expected_mode = u32::from_str_radix(&request.expected_artifact_mode, 8).map_err(|_| {
        ControlError::new(
            "invalid_artifact_mode",
            "expectedArtifactMode must be an octal permission string",
        )
    })?;
    let wrapper_path = release_root.join("bin/buzz-acp");
    if Path::new(&request.acp_command) != wrapper_path {
        return Err(ControlError::new(
            "acp_command_path_mismatch",
            "acpCommand is not the manifest-declared wrapper path",
        ));
    }
    let wrapper_bytes = read_restricted_artifact(
        &wrapper_path,
        owner_uid,
        Some(expected_mode),
        Some(request.expected_acp_command_size),
        "acp_command_validation_failed",
    )?;
    if sha256(&wrapper_bytes) != request.expected_acp_command_sha256 {
        return Err(ControlError::new(
            "acp_command_hash_mismatch",
            "bin/buzz-acp does not match the approved SHA-256",
        ));
    }
    let libexec_path = release_root.join("libexec/buzz-acp");
    let libexec_bytes = read_restricted_artifact(
        &libexec_path,
        owner_uid,
        Some(expected_mode),
        Some(request.expected_libexec_size),
        "libexec_validation_failed",
    )?;
    if sha256(&libexec_bytes) != request.expected_libexec_sha256 {
        return Err(ControlError::new(
            "libexec_hash_mismatch",
            "libexec/buzz-acp does not match the approved SHA-256",
        ));
    }
    if matches!(approved.mcp, McpContract::RuntimeArtifact { .. }) {
        let mcp_path = release_root.join("bin/buzz-dev-mcp");
        if Path::new(&request.mcp_command) != mcp_path {
            return Err(ControlError::new(
                "mcp_command_path_mismatch",
                "mcpCommand is not the manifest-declared runtime path",
            ));
        }
        let mcp_bytes = read_restricted_artifact(
            &mcp_path,
            owner_uid,
            Some(expected_mode),
            Some(request.expected_mcp_command_size),
            "mcp_command_validation_failed",
        )?;
        if sha256(&mcp_bytes) != request.expected_mcp_command_sha256 {
            return Err(ControlError::new(
                "mcp_command_hash_mismatch",
                "bin/buzz-dev-mcp does not match the approved SHA-256",
            ));
        }
    }
    Ok(())
}

fn require_exact_regular_file(
    path: &Path,
    code: &'static str,
    message: &'static str,
) -> Result<(), ControlError> {
    require_exact_canonical_path(path, code)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| ControlError::new(code, message))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ControlError::new(code, message));
    }
    Ok(())
}

fn require_exact_canonical_path(path: &Path, code: &'static str) -> Result<(), ControlError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err(ControlError::new(
            code,
            "path must exist canonically without symlink components",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn expected_owner_uid(owner: &str) -> Result<u32, ControlError> {
    let user = nix::unistd::User::from_name(owner)
        .map_err(|_| {
            ControlError::new(
                "artifact_owner_lookup_failed",
                "failed to resolve the approved artifact owner",
            )
        })?
        .ok_or_else(|| {
            ControlError::new(
                "artifact_owner_lookup_failed",
                "approved artifact owner does not exist",
            )
        })?;
    if user.uid != nix::unistd::geteuid() {
        return Err(ControlError::new(
            "artifact_owner_mismatch",
            "approved artifact owner is not the current user",
        ));
    }
    Ok(user.uid.as_raw())
}

#[cfg(not(unix))]
fn expected_owner_uid(_owner: &str) -> Result<u32, ControlError> {
    Ok(0)
}

fn read_restricted_artifact(
    path: &Path,
    owner_uid: u32,
    expected_mode: Option<u32>,
    expected_size: Option<u64>,
    code: &'static str,
) -> Result<Vec<u8>, ControlError> {
    require_exact_canonical_path(path, code)?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| ControlError::new(code, "failed to inspect runtime artifact"))?;
    validate_artifact_metadata(
        &path_metadata,
        owner_uid,
        expected_mode,
        expected_size,
        code,
    )?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| ControlError::new(code, "failed to securely open runtime artifact"))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| ControlError::new(code, "failed to inspect opened runtime artifact"))?;
    validate_artifact_metadata(
        &file_metadata,
        owner_uid,
        expected_mode,
        expected_size,
        code,
    )?;
    if file_identity(&path_metadata) != file_identity(&file_metadata) {
        return Err(ControlError::new(
            code,
            "runtime artifact changed during secure open",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| ControlError::new(code, "failed to read runtime artifact"))?;
    Ok(bytes)
}

fn validate_artifact_metadata(
    metadata: &fs::Metadata,
    owner_uid: u32,
    expected_mode: Option<u32>,
    expected_size: Option<u64>,
    code: &'static str,
) -> Result<(), ControlError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ControlError::new(
            code,
            "runtime artifact must be a non-symlink regular file",
        ));
    }
    if expected_size.is_some_and(|size| metadata.len() != size) {
        return Err(ControlError::new(
            code,
            "runtime artifact size does not match the approved manifest",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.uid() != owner_uid
            || expected_mode.is_some_and(|expected| mode != expected)
            || mode & 0o022 != 0
        {
            return Err(ControlError::new(
                code,
                "runtime artifact owner or permissions do not match the immutable contract",
            ));
        }
    }
    Ok(())
}

fn read_secure_store(
    path: &Path,
    canonical_store_path: &Path,
) -> Result<SecureStore, ControlError> {
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(STORE_FILENAME)
        || path != canonical_store_path
        || fs::canonicalize(path).ok().as_deref() != Some(path)
    {
        return Err(ControlError::new(
            "invalid_store_path",
            "store must be the exact canonical managed-agents.json path without symlink components",
        ));
    }
    let path_metadata = fs::symlink_metadata(path).map_err(|_| {
        ControlError::new(
            "store_metadata_failed",
            "failed to inspect managed-agent store",
        )
    })?;
    validate_store_metadata(&path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|_| {
        ControlError::new(
            "store_open_failed",
            "failed to securely open managed-agent store",
        )
    })?;
    let file_metadata = file.metadata().map_err(|_| {
        ControlError::new(
            "store_metadata_failed",
            "failed to inspect opened agent store",
        )
    })?;
    validate_store_metadata(&file_metadata)?;
    let path_identity = file_identity(&path_metadata);
    let file_identity = file_identity(&file_metadata);
    if path_identity != file_identity {
        return Err(ControlError::new(
            "store_changed_during_open",
            "managed-agent store changed during secure open",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| {
        ControlError::new("store_read_failed", "failed to read managed-agent store")
    })?;
    Ok(SecureStore {
        bytes,
        identity: file_identity,
    })
}

fn validate_store_metadata(metadata: &fs::Metadata) -> Result<(), ControlError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ControlError::new(
            "invalid_store_file_type",
            "managed-agent store must be a regular file and not a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(ControlError::new(
                "invalid_store_owner",
                "managed-agent store must be owned by the current user",
            ));
        }
        let permissions = metadata.permissions().mode() & 0o777;
        if permissions & !0o600 != 0 {
            return Err(ControlError::new(
                "store_permissions_too_broad",
                "managed-agent store mode must be no broader than 0600",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {}
}

fn build_candidate(bytes: &[u8], request: &ControlRequest) -> Result<Candidate, ControlError> {
    let original: Value = serde_json::from_slice(bytes).map_err(|_| {
        ControlError::new(
            "invalid_store_json",
            "managed-agent store is not a valid JSON array",
        )
    })?;
    let original_records = original.as_array().ok_or_else(|| {
        ControlError::new(
            "invalid_store_shape",
            "managed-agent store must be a JSON array",
        )
    })?;
    let mut keyed_indices: HashMap<&str, usize> = HashMap::new();
    for (index, record) in original_records.iter().enumerate() {
        let object = record.as_object().ok_or_else(|| {
            ControlError::new(
                "invalid_store_record",
                "every managed-agent store record must be a JSON object",
            )
        })?;
        let pubkey = object
            .get("pubkey")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ControlError::new(
                    "invalid_store_pubkey",
                    "every managed-agent store record must contain a string pubkey",
                )
            })?;
        if pubkey.is_empty() {
            continue;
        }
        if !is_lower_hex(pubkey, 64) {
            return Err(ControlError::new(
                "invalid_store_pubkey",
                "every nonempty store public key must be 64 lowercase hexadecimal characters",
            ));
        }
        if keyed_indices.insert(pubkey, index).is_some() {
            return Err(ControlError::new(
                "duplicate_store_pubkey",
                "managed-agent store contains a duplicate nonempty public key",
            ));
        }
    }
    if keyed_indices.len() != request.expected_agent_count {
        return Err(ControlError::new(
            "agent_count_mismatch",
            "managed-agent identity count does not match expectedAgentCount",
        ));
    }
    let requested: HashSet<&str> = request.target_pubkeys.iter().map(String::as_str).collect();
    let stored: HashSet<&str> = keyed_indices.keys().copied().collect();
    if requested != stored {
        return Err(ControlError::new(
            "target_set_mismatch",
            "targetPubkeys must exactly equal all nonempty store identities",
        ));
    }
    let target_indices: Vec<usize> = request
        .target_pubkeys
        .iter()
        .filter_map(|target| keyed_indices.get(target.as_str()).copied())
        .collect();
    let mut candidate = original.clone();
    {
        let candidate_records = candidate.as_array_mut().ok_or_else(diff_error)?;
        for index in &target_indices {
            let object = candidate_records[*index]
                .as_object_mut()
                .ok_or_else(diff_error)?;
            patch_record(object, request)?;
        }
    }
    validate_semantic_diff(&original, &candidate, &target_indices)?;
    let candidate_records = candidate.as_array().ok_or_else(diff_error)?;
    let (changed_fields, changed_env_keys) =
        collect_changed_names(original_records, candidate_records, &target_indices)?;
    let mut candidate_bytes = serde_json::to_vec_pretty(&candidate).map_err(|_| {
        ControlError::new(
            "store_serialization_failed",
            "failed to serialize validated managed-agent candidate",
        )
    })?;
    if bytes.ends_with(b"\n") {
        candidate_bytes.push(b'\n');
    }
    Ok(Candidate {
        bytes: candidate_bytes,
        changed_fields,
        changed_env_keys,
        agent_count: keyed_indices.len(),
    })
}

fn patch_record(
    object: &mut Map<String, Value>,
    request: &ControlRequest,
) -> Result<(), ControlError> {
    object.insert(
        "acp_command".to_owned(),
        Value::String(request.acp_command.clone()),
    );
    object.insert(
        "mcp_command".to_owned(),
        Value::String(request.mcp_command.clone()),
    );
    object.insert(
        "parallelism".to_owned(),
        Value::Number(request.parallelism.into()),
    );
    let env = object
        .entry("env_vars".to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            ControlError::new(
                "invalid_store_env_vars",
                "target env_vars must be a JSON object when present",
            )
        })?;
    for key in &request.env_unset {
        env.remove(key);
    }
    for (key, value) in &request.env_set {
        env.insert(key.clone(), Value::String(value.clone()));
    }
    Ok(())
}

fn validate_semantic_diff(
    original: &Value,
    candidate: &Value,
    target_indices: &[usize],
) -> Result<(), ControlError> {
    let original_records = original.as_array().ok_or_else(diff_error)?;
    let candidate_records = candidate.as_array().ok_or_else(diff_error)?;
    if original_records.len() != candidate_records.len() {
        return Err(diff_error());
    }
    let targets: HashSet<usize> = target_indices.iter().copied().collect();
    for (index, (before, after)) in original_records
        .iter()
        .zip(candidate_records.iter())
        .enumerate()
    {
        if !targets.contains(&index) {
            if before != after {
                return Err(diff_error());
            }
            continue;
        }
        let before_object = before.as_object().ok_or_else(diff_error)?;
        let after_object = after.as_object().ok_or_else(diff_error)?;
        if protected_top_level(before_object) != protected_top_level(after_object)
            || protected_env(before_object)? != protected_env(after_object)?
        {
            return Err(diff_error());
        }
    }
    Ok(())
}

fn protected_top_level(object: &Map<String, Value>) -> Map<String, Value> {
    let mut protected = object.clone();
    for key in ["acp_command", "mcp_command", "parallelism", "env_vars"] {
        protected.remove(key);
    }
    protected
}

fn protected_env(object: &Map<String, Value>) -> Result<Map<String, Value>, ControlError> {
    let mut env = env_object_or_empty(object)?;
    for key in ALLOWED_ENV_KEYS {
        env.remove(key);
    }
    Ok(env)
}

fn collect_changed_names(
    original: &[Value],
    candidate: &[Value],
    targets: &[usize],
) -> Result<(Vec<String>, Vec<String>), ControlError> {
    let mut fields = BTreeSet::new();
    let mut env_keys = BTreeSet::new();
    for index in targets {
        let before = original[*index].as_object().ok_or_else(diff_error)?;
        let after = candidate[*index].as_object().ok_or_else(diff_error)?;
        for field in ["acp_command", "mcp_command", "parallelism", "env_vars"] {
            if before.get(field) != after.get(field) {
                fields.insert(field.to_owned());
            }
        }
        let before_env = env_object_or_empty(before)?;
        let after_env = env_object_or_empty(after)?;
        for key in ALLOWED_ENV_KEYS {
            if before_env.get(key) != after_env.get(key) {
                env_keys.insert(key.to_owned());
            }
        }
    }
    Ok((fields.into_iter().collect(), env_keys.into_iter().collect()))
}

fn env_object_or_empty(object: &Map<String, Value>) -> Result<Map<String, Value>, ControlError> {
    match object.get("env_vars") {
        None => Ok(Map::new()),
        Some(Value::Object(env)) => Ok(env.clone()),
        Some(_) => Err(ControlError::new(
            "invalid_store_env_vars",
            "target env_vars must be a JSON object when present",
        )),
    }
}

fn diff_error() -> ControlError {
    ControlError::new(
        "candidate_diff_rejected",
        "candidate changes data outside the bounded target patch",
    )
}

fn stage_restricted_file(
    destination: &Path,
    bytes: &[u8],
) -> Result<tempfile::NamedTempFile, ControlError> {
    let parent = destination.parent().ok_or_else(|| {
        ControlError::new(
            "invalid_write_destination",
            "write destination must have a parent directory",
        )
    })?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|_| {
        ControlError::new(
            "stage_create_failed",
            "failed to stage restricted atomic write",
        )
    })?;
    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| {
            ControlError::new(
                "stage_permissions_failed",
                "failed to restrict staged file permissions",
            )
        })?;
    staged
        .write_all(bytes)
        .map_err(|_| ControlError::new("stage_write_failed", "failed to write staged candidate"))?;
    staged.as_file().sync_all().map_err(|_| {
        ControlError::new(
            "stage_sync_failed",
            "failed to durably sync staged candidate",
        )
    })?;
    Ok(staged)
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn commit_staged_file(
    staged: tempfile::NamedTempFile,
    destination: &Path,
    code: &'static str,
) -> Result<(), ControlError> {
    staged
        .persist(destination)
        .map_err(|_| ControlError::new(code, "failed to atomically replace destination"))?;
    if let Some(parent) = destination.parent() {
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
    }
    Ok(())
}

fn is_allowed_env_key(key: &str) -> bool {
    ALLOWED_ENV_KEYS.contains(&key)
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
