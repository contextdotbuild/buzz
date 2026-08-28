//! A deliberately narrow offline control surface for Buzz-managed agents.
//!
//! The binary edits an exact `managed-agents.json` only while the caller's
//! expected Desktop PID is gone. It preserves opaque records as JSON values so
//! credentials and prompts never become output-bearing typed fields.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const REQUIRED_PARALLELISM: u32 = 10;
const PRODUCTION_RUNTIME_ROOT: &str = "/Users/timi/.buzz/RUNTIMES/buzz-heartbeat";
const STORE_FILENAME: &str = "managed-agents.json";
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
    #[serde(alias = "targetPublicKeys")]
    target_pubkeys: Vec<String>,
    acp_command: String,
    parallelism: u32,
    #[serde(default)]
    env_set: BTreeMap<String, String>,
    #[serde(default)]
    env_unset: Vec<String>,
    #[serde(default)]
    receipt_path: Option<PathBuf>,
}

/// Secret-free success or dry-run receipt.
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
    agent_count: usize,
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
    execute_with_context(
        options,
        &ExecutionContext {
            runtime_root: Path::new(PRODUCTION_RUNTIME_ROOT),
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

    validate_request(&request)?;
    require_expected_desktop_stopped(request.expected_desktop_pid)?;
    validate_acp_command(&request.acp_command, context.runtime_root)?;
    validate_receipt_path(request.receipt_path.as_deref(), &options.store_path)?;

    let store = read_secure_store(&options.store_path)?;
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
        agent_count: candidate.agent_count,
    };

    if options.dry_run {
        return Ok(receipt);
    }

    let staged_store = stage_restricted_file(&options.store_path, &candidate.bytes)?;
    let staged_receipt = match request.receipt_path.as_deref() {
        Some(path) => {
            let mut bytes = serde_json::to_vec_pretty(&receipt).map_err(|_| {
                ControlError::new(
                    "receipt_serialization_failed",
                    "failed to serialize receipt",
                )
            })?;
            bytes.push(b'\n');
            Some(stage_restricted_file(path, &bytes)?)
        }
        None => None,
    };

    // The Desktop is stopped, but this last read closes the ordinary stale
    // hash / concurrent writer window before the single atomic replacement.
    let current = read_secure_store(&options.store_path)?;
    if current.identity != store.identity || sha256(&current.bytes) != receipt.actual_before_sha256
    {
        return Err(ControlError::new(
            "store_changed_before_commit",
            "store changed after validation; no mutation was applied",
        ));
    }
    require_expected_desktop_stopped(request.expected_desktop_pid)?;

    commit_staged_file(staged_store, &options.store_path, "store_commit_failed")?;
    if let (Some(staged), Some(receipt_path)) = (staged_receipt, request.receipt_path.as_deref()) {
        // The store is already durably committed. A fully staged receipt can
        // only fail here on an external filesystem change. Do not turn an
        // applied store update into a false nonzero retry signal.
        let _ = commit_staged_file(staged, receipt_path, "receipt_commit_failed");
    }
    Ok(receipt)
}

fn validate_request(request: &ControlRequest) -> Result<(), ControlError> {
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
    if request.target_pubkeys.is_empty() {
        return Err(ControlError::new(
            "empty_target_list",
            "targetPubkeys must contain at least one public key",
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
    Ok(())
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

fn validate_acp_command(command: &str, runtime_root: &Path) -> Result<(), ControlError> {
    let path = Path::new(command);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ControlError::new(
            "invalid_acp_command",
            "acpCommand must be an absolute canonical path",
        ));
    }

    let relative = path.strip_prefix(runtime_root).map_err(|_| {
        ControlError::new(
            "invalid_acp_command_root",
            "acpCommand must be beneath the approved heartbeat runtime root",
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() < 2 {
        return Err(ControlError::new(
            "invalid_acp_command_layout",
            "acpCommand does not identify a commit-pinned buzz-acp executable",
        ));
    }
    let release_id = component_text(components[0]).ok_or_else(|| {
        ControlError::new(
            "invalid_acp_command_layout",
            "acpCommand release component is invalid",
        )
    })?;
    if !is_lower_hex(release_id, 40) {
        return Err(ControlError::new(
            "invalid_acp_release_id",
            "acpCommand release must be a 40-character lowercase commit id",
        ));
    }
    let suffix: Vec<&str> = components[1..]
        .iter()
        .map(|component| component_text(*component))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            ControlError::new(
                "invalid_acp_command_layout",
                "acpCommand executable suffix is invalid",
            )
        })?;
    let allowed_suffix = matches!(
        suffix.as_slice(),
        ["buzz-acp"] | ["bin", "buzz-acp"] | ["libexec", "buzz-acp"]
    );
    if !allowed_suffix {
        return Err(ControlError::new(
            "invalid_acp_command_layout",
            "acpCommand must select the release buzz-acp executable",
        ));
    }

    let release_path = runtime_root.join(release_id);
    let canonical_runtime_root = fs::canonicalize(runtime_root).map_err(|_| {
        ControlError::new(
            "acp_runtime_root_not_found",
            "approved heartbeat runtime root does not exist",
        )
    })?;
    let canonical_release = fs::canonicalize(&release_path).map_err(|_| {
        ControlError::new(
            "acp_release_not_found",
            "acpCommand release directory does not exist",
        )
    })?;
    if canonical_release != canonical_runtime_root.join(release_id) {
        return Err(ControlError::new(
            "acp_release_symlink_escape",
            "acpCommand release resolves outside its exact commit-pinned directory",
        ));
    }
    let canonical_command = fs::canonicalize(path).map_err(|_| {
        ControlError::new(
            "acp_command_not_found",
            "acpCommand executable does not exist",
        )
    })?;
    if !canonical_command.starts_with(&canonical_release) {
        return Err(ControlError::new(
            "acp_command_symlink_escape",
            "acpCommand resolves outside its commit-pinned release",
        ));
    }
    if canonical_command.file_name().and_then(|name| name.to_str()) != Some("buzz-acp") {
        return Err(ControlError::new(
            "invalid_acp_command_layout",
            "acpCommand must resolve to a file named buzz-acp",
        ));
    }
    let metadata = fs::metadata(&canonical_command).map_err(|_| {
        ControlError::new(
            "acp_command_metadata_failed",
            "failed to inspect acpCommand executable",
        )
    })?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(ControlError::new(
            "acp_command_not_executable",
            "acpCommand must resolve to an executable regular file",
        ));
    }
    Ok(())
}

fn component_text(component: Component<'_>) -> Option<&str> {
    match component {
        Component::Normal(value) => value.to_str(),
        _ => None,
    }
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

fn validate_receipt_path(path: Option<&Path>, store_path: &Path) -> Result<(), ControlError> {
    let Some(path) = path else {
        return Ok(());
    };
    if !path.is_absolute() {
        return Err(ControlError::new(
            "invalid_receipt_path",
            "receiptPath must be absolute",
        ));
    }
    if path == store_path {
        return Err(ControlError::new(
            "invalid_receipt_path",
            "receiptPath must not be the managed-agent store",
        ));
    }
    validate_existing_destination(path, "invalid_receipt_path")
}

fn read_secure_store(path: &Path) -> Result<SecureStore, ControlError> {
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(STORE_FILENAME)
    {
        return Err(ControlError::new(
            "invalid_store_path",
            "store must be an absolute path ending in managed-agents.json",
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

    let mut keyed_indices: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut agent_count = 0usize;
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
        if !pubkey.is_empty() {
            agent_count += 1;
            keyed_indices.entry(pubkey).or_default().push(index);
        }
    }
    if keyed_indices.values().any(|indices| indices.len() != 1) {
        return Err(ControlError::new(
            "duplicate_store_pubkey",
            "managed-agent store contains a duplicate nonempty public key",
        ));
    }
    if agent_count != request.expected_agent_count {
        return Err(ControlError::new(
            "agent_count_mismatch",
            "managed-agent identity count does not match expectedAgentCount",
        ));
    }

    let mut target_indices = Vec::with_capacity(request.target_pubkeys.len());
    for target in &request.target_pubkeys {
        let indices = keyed_indices.get(target.as_str()).ok_or_else(|| {
            ControlError::new(
                "target_not_found",
                "a requested target public key is absent from the store",
            )
        })?;
        target_indices.push(indices[0]);
    }

    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut candidate = original.clone();
    {
        let candidate_records = candidate.as_array_mut().ok_or_else(|| {
            ControlError::new(
                "invalid_store_shape",
                "managed-agent store must be a JSON array",
            )
        })?;
        for index in &target_indices {
            let object = candidate_records[*index].as_object_mut().ok_or_else(|| {
                ControlError::new(
                    "invalid_store_record",
                    "target managed-agent record must be a JSON object",
                )
            })?;
            patch_record(object, request, &timestamp)?;
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
        agent_count,
    })
}

fn patch_record(
    object: &mut Map<String, Value>,
    request: &ControlRequest,
    timestamp: &str,
) -> Result<(), ControlError> {
    object.insert(
        "acp_command".to_owned(),
        Value::String(request.acp_command.clone()),
    );
    object.insert(
        "parallelism".to_owned(),
        Value::Number(request.parallelism.into()),
    );

    if !request.env_set.is_empty() || !request.env_unset.is_empty() {
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
    } else if object
        .get("env_vars")
        .is_some_and(|value| !value.is_object())
    {
        return Err(ControlError::new(
            "invalid_store_env_vars",
            "target env_vars must be a JSON object when present",
        ));
    }
    object.insert("updated_at".to_owned(), Value::String(timestamp.to_owned()));
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
        if before_object.get("pubkey") != after_object.get("pubkey") {
            return Err(diff_error());
        }

        let before_protected = protected_top_level(before_object);
        let after_protected = protected_top_level(after_object);
        if before_protected != after_protected {
            return Err(diff_error());
        }
        if protected_env(before_object)? != protected_env(after_object)? {
            return Err(diff_error());
        }
    }
    Ok(())
}

fn protected_top_level(object: &Map<String, Value>) -> Map<String, Value> {
    let mut protected = object.clone();
    for key in ["acp_command", "parallelism", "updated_at", "env_vars"] {
        protected.remove(key);
    }
    protected
}

fn protected_env(object: &Map<String, Value>) -> Result<Map<String, Value>, ControlError> {
    let mut env = match object.get("env_vars") {
        None => Map::new(),
        Some(Value::Object(env)) => env.clone(),
        Some(_) => {
            return Err(ControlError::new(
                "invalid_store_env_vars",
                "target env_vars must be a JSON object when present",
            ))
        }
    };
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
        for field in ["acp_command", "parallelism", "updated_at", "env_vars"] {
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

fn validate_existing_destination(path: &Path, code: &'static str) -> Result<(), ControlError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ControlError::new(
                    code,
                    "existing destination must be a regular file and not a symlink",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.uid() != nix::unistd::geteuid().as_raw()
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(ControlError::new(
                        code,
                        "existing destination must be current-user owned and owner-only",
                    ));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| {
                ControlError::new(code, "destination must have an existing parent directory")
            })?;
            if !parent.is_dir() {
                return Err(ControlError::new(
                    code,
                    "destination parent directory does not exist",
                ));
            }
            Ok(())
        }
        Err(_) => Err(ControlError::new(
            code,
            "failed to inspect destination path",
        )),
    }
}

fn stage_restricted_file(
    destination: &Path,
    bytes: &[u8],
) -> Result<tempfile::NamedTempFile, ControlError> {
    validate_existing_destination(destination, "invalid_write_destination")?;
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
    // The staged regular file was already mode-0600 and fsync'd. Once rename
    // succeeds, do not manufacture a failure that could prompt an unsafe
    // retry after mutation. Directory fsync is a best-effort durability
    // reinforcement because some filesystems do not support it.
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
