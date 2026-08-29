//! Owner-only, read-only Buzz conversation operator.
//!
//! The public `buzz-read` process only writes a bounded, non-secret request and
//! asks launchd to execute the hidden helper in the logged-in GUI session. The
//! helper is part of the Buzz Desktop crate: it reads the existing human
//! identity inside the desktop keyring boundary, performs one authenticated
//! relay query, and writes a bounded mode-0600 receipt. The private key and the
//! NIP-98 Authorization event never cross that process boundary.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime},
};

use chrono::DateTime;
use futures_util::StreamExt;
use nostr::{Event, Keys};
use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::Zeroizing;

const SCHEMA_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_RELAY_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECEIPT_BYTES: usize = 128 * 1024;
const MAX_RESULTS: u32 = 100;
const MAX_EXCERPT_CHARS: u32 = 512;
const MAX_SEARCH_CHARS: usize = 256;
const MAX_RANGE_SECONDS: i64 = 31 * 24 * 60 * 60;
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MESSAGE_KINDS: [u32; 4] = [9, 40002, 45001, 45003];
const IDENTITY_KEY_NAME: &str = "identity";
const CONTROL_DIR_NAME: &str = "operator-read";
const HELPER_FLAG: &str = "--buzz-read-helper";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    schema_version: u32,
    request_id: String,
    operation: String,
    relay: String,
    since: i64,
    until: i64,
    limit: u32,
    excerpt_chars: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadReceipt {
    schema_version: u32,
    request_id: String,
    status: String,
    operation: String,
    generated_at: i64,
    relay_host: String,
    identity_pubkey: String,
    requested_limit: u32,
    returned: usize,
    truncated: bool,
    events: Vec<ReceiptEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReceiptEvent {
    id: String,
    author_pubkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_name: Option<String>,
    kind: u32,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<String>,
}

#[derive(Debug)]
struct OperatorError {
    code: &'static str,
    message: &'static str,
}

impl OperatorError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

struct ClientPaths {
    root: PathBuf,
    requests: PathBuf,
    receipts: PathBuf,
    launch_agents: PathBuf,
}

impl ClientPaths {
    fn resolve() -> Result<Self, OperatorError> {
        let home = dirs::home_dir().ok_or_else(|| {
            OperatorError::new(
                "home_unavailable",
                "could not resolve the current user home",
            )
        })?;
        let root = home.join(".buzz").join(CONTROL_DIR_NAME);
        Ok(Self {
            requests: root.join("requests"),
            receipts: root.join("receipts"),
            launch_agents: root.join("launch-agents"),
            root,
        })
    }

    fn ensure(&self) -> Result<(), OperatorError> {
        ensure_owner_only_dir(&self.root)?;
        ensure_owner_only_dir(&self.requests)?;
        ensure_owner_only_dir(&self.receipts)?;
        ensure_owner_only_dir(&self.launch_agents)?;
        Ok(())
    }
}

struct LaunchCleanup {
    label: String,
    domain: String,
    request_path: PathBuf,
    plist_path: PathBuf,
}

impl Drop for LaunchCleanup {
    fn drop(&mut self) {
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &format!("{}/{}", self.domain, self.label)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_file(&self.request_path);
        let _ = fs::remove_file(&self.plist_path);
    }
}

/// Entry point for the installed `buzz-read` binary.
///
/// The ordinary mode accepts only the `messages` read operation. The hidden
/// helper mode is invoked by the ordinary mode through a one-shot GUI
/// LaunchAgent and requires owner-only request and receipt paths.
pub fn run_operator_read_cli<I>(args: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let result = if args.get(1).and_then(|value| value.to_str()) == Some(HELPER_FLAG) {
        run_helper_args(&args)
    } else {
        run_client_args(&args)
    };

    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("buzz-read: {}", error.message);
            1
        }
    }
}

fn run_client_args(args: &[OsString]) -> Result<(), OperatorError> {
    let request = parse_client_args(args)?;
    validate_request(&request)?;

    let paths = ClientPaths::resolve()?;
    paths.ensure()?;
    prune_old_receipts(&paths.receipts);

    let request_path = paths.requests.join(format!("{}.json", request.request_id));
    let receipt_path = paths.receipts.join(format!("{}.json", request.request_id));
    let plist_path = paths
        .launch_agents
        .join(format!("{}.plist", request.request_id));
    write_new_owner_only_json(&request_path, &request)?;

    let helper = std::env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok())
        .ok_or_else(|| {
            OperatorError::new(
                "helper_unavailable",
                "could not resolve the installed helper",
            )
        })?;
    if !helper.is_absolute() {
        return Err(OperatorError::new(
            "helper_unavailable",
            "the installed helper path is not absolute",
        ));
    }

    let uid = current_uid();
    let domain = format!("gui/{uid}");
    let label = format!(
        "xyz.block.buzz.operator-read.{}",
        request.request_id.replace('-', "")
    );
    write_launch_agent(
        &plist_path,
        &label,
        &helper,
        &request_path,
        &receipt_path,
        &paths.root,
    )?;
    let _cleanup = LaunchCleanup {
        label: label.clone(),
        domain: domain.clone(),
        request_path,
        plist_path: plist_path.clone(),
    };

    let status = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| {
            OperatorError::new(
                "gui_bridge_unavailable",
                "could not start the Buzz read helper in the GUI session",
            )
        })?;
    if !status.success() {
        return Err(OperatorError::new(
            "gui_bridge_unavailable",
            "the Buzz GUI read bridge refused the request",
        ));
    }

    wait_for_receipt(&receipt_path)?;
    let bytes = read_bounded_owner_only_file(&receipt_path, MAX_RECEIPT_BYTES as u64)?;
    let receipt: ReadReceipt = serde_json::from_slice(&bytes).map_err(|_| {
        OperatorError::new("receipt_invalid", "the Buzz read receipt was malformed")
    })?;
    if receipt.request_id != request.request_id {
        return Err(OperatorError::new(
            "receipt_invalid",
            "the Buzz read receipt did not match the request",
        ));
    }
    let output = serde_json::to_vec_pretty(&receipt).map_err(|_| {
        OperatorError::new(
            "receipt_invalid",
            "could not serialize the Buzz read receipt",
        )
    })?;
    std::io::stdout()
        .write_all(&output)
        .and_then(|_| std::io::stdout().write_all(b"\n"))
        .map_err(|_| OperatorError::new("output_failed", "could not write the read receipt"))?;
    if receipt.status != "ok" {
        return Err(OperatorError::new(
            "read_failed",
            "Buzz Desktop could not complete the authenticated read",
        ));
    }
    Ok(())
}

fn run_helper_args(args: &[OsString]) -> Result<(), OperatorError> {
    if args.len() != 4 {
        return Err(OperatorError::new(
            "helper_invalid",
            "the Buzz read helper request was invalid",
        ));
    }
    reject_secret_environment()?;
    let request_path = PathBuf::from(&args[2]);
    let receipt_path = PathBuf::from(&args[3]);
    let paths = ClientPaths::resolve()?;
    validate_helper_paths(&paths, &request_path, &receipt_path)?;

    let raw = read_bounded_owner_only_file(&request_path, MAX_REQUEST_BYTES)?;
    let request_id = request_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
        .to_string();
    let outcome = serde_json::from_slice::<ReadRequest>(&raw)
        .map_err(|_| OperatorError::new("request_invalid", "the read request was malformed"))
        .and_then(|request| {
            validate_request(&request)?;
            if request.request_id != request_id {
                return Err(OperatorError::new(
                    "request_invalid",
                    "the read request id did not match its control file",
                ));
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| {
                    OperatorError::new(
                        "runtime_unavailable",
                        "could not start the Buzz read runtime",
                    )
                })?;
            runtime.block_on(execute_read(request))
        });

    let receipt = match outcome {
        Ok(receipt) => receipt,
        Err(error) => error_receipt(request_id, &error),
    };
    write_receipt_atomic(&receipt_path, &receipt)?;
    if receipt.status == "ok" {
        Ok(())
    } else {
        Err(OperatorError::new(
            "read_failed",
            "the authenticated Buzz read failed",
        ))
    }
}

fn parse_client_args(args: &[OsString]) -> Result<ReadRequest, OperatorError> {
    if args.len() < 2 || args[1].to_str() != Some("messages") {
        return Err(OperatorError::new(
            "usage",
            "usage: buzz-read messages --relay <wss-url> --since <RFC3339|unix> --until <RFC3339|unix> [--channel <uuid>] [--search <text>] [--limit 1..100] [--excerpt-chars 0..512]",
        ));
    }

    let mut values: HashMap<&str, String> = HashMap::new();
    let mut index = 2;
    while index < args.len() {
        let flag = args[index]
            .to_str()
            .ok_or_else(|| OperatorError::new("usage", "arguments must be valid UTF-8"))?;
        let known = matches!(
            flag,
            "--relay"
                | "--since"
                | "--until"
                | "--channel"
                | "--search"
                | "--limit"
                | "--excerpt-chars"
        );
        if !known || values.contains_key(flag) || index + 1 >= args.len() {
            return Err(OperatorError::new(
                "usage",
                "the Buzz read arguments were invalid or duplicated",
            ));
        }
        let value = args[index + 1]
            .to_str()
            .ok_or_else(|| OperatorError::new("usage", "argument values must be valid UTF-8"))?;
        values.insert(flag, value.to_string());
        index += 2;
    }

    let relay = required_value(&values, "--relay")?;
    let since = parse_time(&required_value(&values, "--since")?)?;
    let until = parse_time(&required_value(&values, "--until")?)?;
    let limit = optional_u32(&values, "--limit")?.unwrap_or(50);
    let excerpt_chars = optional_u32(&values, "--excerpt-chars")?.unwrap_or(280);
    Ok(ReadRequest {
        schema_version: SCHEMA_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        operation: "messages".to_string(),
        relay,
        since,
        until,
        limit,
        excerpt_chars,
        channel: values.get("--channel").cloned(),
        search: values.get("--search").cloned(),
    })
}

fn required_value(values: &HashMap<&str, String>, name: &str) -> Result<String, OperatorError> {
    values
        .get(name)
        .cloned()
        .ok_or_else(|| OperatorError::new("usage", "relay, since, and until are required"))
}

fn optional_u32(values: &HashMap<&str, String>, name: &str) -> Result<Option<u32>, OperatorError> {
    values
        .get(name)
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| OperatorError::new("usage", "numeric options were invalid"))
        })
        .transpose()
}

fn parse_time(value: &str) -> Result<i64, OperatorError> {
    value.parse::<i64>().or_else(|_| {
        DateTime::parse_from_rfc3339(value)
            .map(|time| time.timestamp())
            .map_err(|_| OperatorError::new("usage", "time must be RFC3339 or Unix seconds"))
    })
}

fn validate_request(request: &ReadRequest) -> Result<(), OperatorError> {
    if request.schema_version != SCHEMA_VERSION || request.operation != "messages" {
        return Err(OperatorError::new(
            "operation_rejected",
            "only the version-1 messages read operation is available",
        ));
    }
    uuid::Uuid::parse_str(&request.request_id)
        .map_err(|_| OperatorError::new("request_invalid", "request_id must be a UUID"))?;
    validate_relay(&request.relay)?;
    if request.since <= 0
        || request.until <= request.since
        || request.until - request.since > MAX_RANGE_SECONDS
    {
        return Err(OperatorError::new(
            "range_rejected",
            "the requested date range must be positive, ordered, and no longer than 31 days",
        ));
    }
    if request.limit == 0 || request.limit > MAX_RESULTS {
        return Err(OperatorError::new(
            "limit_rejected",
            "the requested limit must be between 1 and 100",
        ));
    }
    if request.excerpt_chars > MAX_EXCERPT_CHARS {
        return Err(OperatorError::new(
            "limit_rejected",
            "excerpt_chars must be between 0 and 512",
        ));
    }
    if let Some(channel) = request.channel.as_deref() {
        uuid::Uuid::parse_str(channel)
            .map_err(|_| OperatorError::new("channel_rejected", "channel must be a UUID"))?;
    }
    if let Some(search) = request.search.as_deref() {
        if search.trim().is_empty()
            || search.chars().count() > MAX_SEARCH_CHARS
            || search.chars().any(char::is_control)
        {
            return Err(OperatorError::new(
                "search_rejected",
                "search text must be non-empty, printable, and at most 256 characters",
            ));
        }
    }
    Ok(())
}

fn validate_relay(value: &str) -> Result<Url, OperatorError> {
    let parsed = Url::parse(value)
        .map_err(|_| OperatorError::new("relay_rejected", "relay must be a valid URL"))?;
    if !matches!(parsed.scheme(), "wss" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some()
    {
        return Err(OperatorError::new(
            "relay_rejected",
            "relay must be a credential-free wss or https origin",
        ));
    }
    Ok(parsed)
}

async fn execute_read(request: ReadRequest) -> Result<ReadReceipt, OperatorError> {
    let relay = validate_relay(&request.relay)?;
    let relay_host = relay.host_str().unwrap_or_default().to_string();
    let keys = load_identity_readonly()?;
    let identity_pubkey = keys.public_key().to_hex();

    let mut filter = serde_json::json!({
        "kinds": MESSAGE_KINDS,
        "since": request.since,
        "until": request.until,
        "limit": request.limit,
    });
    if let Some(channel) = request.channel.as_deref() {
        filter["#h"] = serde_json::json!([channel]);
    }
    if let Some(search) = request.search.as_deref() {
        filter["search"] = serde_json::json!(search.trim());
    }

    let mut events = query_bounded(&keys, &request.relay, &filter).await?;
    events.retain(|event| event_matches_request(event, &request));
    events.sort_by(|left, right| {
        right
            .created_at
            .as_secs()
            .cmp(&left.created_at.as_secs())
            .then_with(|| left.id.to_hex().cmp(&right.id.to_hex()))
    });
    let mut seen = HashSet::new();
    events.retain(|event| seen.insert(event.id.to_hex()));
    let truncated = events.len() > request.limit as usize;
    events.truncate(request.limit as usize);

    let author_names = fetch_author_names(&keys, &request.relay, &events)
        .await
        .unwrap_or_default();
    let projected = events
        .iter()
        .map(|event| project_event(event, request.excerpt_chars, &author_names))
        .collect::<Vec<_>>();
    let receipt = ReadReceipt {
        schema_version: SCHEMA_VERSION,
        request_id: request.request_id,
        status: "ok".to_string(),
        operation: "messages".to_string(),
        generated_at: chrono::Utc::now().timestamp(),
        relay_host,
        identity_pubkey,
        requested_limit: request.limit,
        returned: projected.len(),
        truncated,
        events: projected,
        error_code: None,
        message: None,
    };
    ensure_receipt_bound(&receipt)?;
    Ok(receipt)
}

fn load_identity_readonly() -> Result<Keys, OperatorError> {
    let store = crate::secret_store::SecretStore::keyring(crate::app_state::keyring_service());
    let nsec = store
        .load_readonly(IDENTITY_KEY_NAME)
        .map_err(|_| {
            OperatorError::new(
                "identity_unavailable",
                "Buzz Desktop identity storage was unavailable",
            )
        })?
        .ok_or_else(|| {
            OperatorError::new(
                "identity_unavailable",
                "Buzz Desktop has no current keyring identity",
            )
        })?;
    let nsec = Zeroizing::new(nsec);
    Keys::parse(nsec.trim()).map_err(|_| {
        OperatorError::new(
            "identity_unavailable",
            "Buzz Desktop identity storage was invalid",
        )
    })
}

async fn query_bounded(
    keys: &Keys,
    relay: &str,
    filter: &serde_json::Value,
) -> Result<Vec<Event>, OperatorError> {
    let mut base = validate_relay(relay)?;
    base.set_scheme("https").map_err(|_| {
        OperatorError::new("relay_rejected", "relay could not be converted to https")
    })?;
    base.set_path("/query");
    let url = base.as_str().to_string();
    let body = serde_json::to_vec(&[filter]).map_err(|_| {
        OperatorError::new(
            "request_invalid",
            "the relay filter could not be serialized",
        )
    })?;
    let auth = crate::relay::build_nip98_auth_header_for_keys(keys, &Method::POST, &url, &body)
        .map_err(|_| {
            OperatorError::new(
                "identity_unavailable",
                "Buzz Desktop could not authenticate the read",
            )
        })?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            OperatorError::new("relay_unavailable", "the Buzz relay client was unavailable")
        })?;
    let response = client
        .post(url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(30))
        .body(body)
        .send()
        .await
        .map_err(|_| {
            OperatorError::new("relay_unavailable", "the Buzz relay could not be reached")
        })?;
    if !response.status().is_success() {
        return Err(OperatorError::new(
            "relay_rejected",
            "the Buzz relay rejected the authenticated read",
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RELAY_RESPONSE_BYTES as u64)
    {
        return Err(OperatorError::new(
            "response_oversize",
            "the Buzz relay response exceeded the read bound",
        ));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            OperatorError::new(
                "relay_unavailable",
                "the Buzz relay response was interrupted",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RELAY_RESPONSE_BYTES {
            return Err(OperatorError::new(
                "response_oversize",
                "the Buzz relay response exceeded the read bound",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let events: Vec<Event> = serde_json::from_slice(&bytes).map_err(|_| {
        OperatorError::new("response_invalid", "the Buzz relay response was malformed")
    })?;
    if events.len() > MAX_RESULTS as usize {
        return Err(OperatorError::new(
            "response_oversize",
            "the Buzz relay returned more events than requested",
        ));
    }
    for event in &events {
        event.verify().map_err(|_| {
            OperatorError::new(
                "response_unverified",
                "the Buzz relay returned an event that failed verification",
            )
        })?;
    }
    Ok(events)
}

async fn fetch_author_names(
    keys: &Keys,
    relay: &str,
    events: &[Event],
) -> Result<HashMap<String, String>, OperatorError> {
    let authors = events
        .iter()
        .map(|event| event.pubkey.to_hex())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if authors.is_empty() {
        return Ok(HashMap::new());
    }
    let filter = serde_json::json!({
        "kinds": [0],
        "authors": authors,
        "limit": authors.len().min(MAX_RESULTS as usize),
    });
    let profiles = query_bounded(keys, relay, &filter).await?;
    let mut names = HashMap::new();
    for profile in profiles {
        let pubkey = profile.pubkey.to_hex();
        if names.contains_key(&pubkey) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&profile.content) else {
            continue;
        };
        let name = value
            .get("display_name")
            .or_else(|| value.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.chars().count() <= 80)
            .map(redact_sensitive_text);
        if let Some(name) = name {
            names.insert(pubkey, name);
        }
    }
    Ok(names)
}

fn event_matches_request(event: &Event, request: &ReadRequest) -> bool {
    let kind = event.kind.as_u16() as u32;
    let created_at = event.created_at.as_secs() as i64;
    MESSAGE_KINDS.contains(&kind)
        && created_at >= request.since
        && created_at <= request.until
        && request
            .channel
            .as_deref()
            .is_none_or(|expected| event_channel(event).as_deref() == Some(expected))
}

fn project_event(
    event: &Event,
    excerpt_chars: u32,
    author_names: &HashMap<String, String>,
) -> ReceiptEvent {
    let author_pubkey = event.pubkey.to_hex();
    let excerpt = (excerpt_chars > 0).then(|| bounded_excerpt(&event.content, excerpt_chars));
    ReceiptEvent {
        id: event.id.to_hex(),
        author_name: author_names.get(&author_pubkey).cloned(),
        author_pubkey,
        kind: event.kind.as_u16() as u32,
        created_at: event.created_at.as_secs() as i64,
        channel: event_channel(event),
        excerpt,
    }
}

fn event_channel(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let values = tag.as_slice();
        (values.first().map(String::as_str) == Some("h"))
            .then(|| values.get(1).cloned())
            .flatten()
    })
}

fn bounded_excerpt(content: &str, max_chars: u32) -> String {
    let normalized = content
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut excerpt = normalized
        .chars()
        .take(max_chars as usize)
        .collect::<String>();
    if normalized.chars().count() > max_chars as usize {
        excerpt.push('…');
    }
    redact_sensitive_text(&excerpt)
}

fn redact_sensitive_text(input: &str) -> String {
    static PATTERNS: std::sync::OnceLock<Vec<(Regex, &'static str)>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            (
                Regex::new(r"(?i)nsec1[023456789acdefghjklmnpqrstuvwxyz]{16,}")
                    .expect("static nsec regex"),
                "[REDACTED_NSEC]",
            ),
            (
                Regex::new(r"(?i)(?:sk-|ghp_|github_pat_|xox[baprs]-)[A-Za-z0-9_-]{10,}")
                    .expect("static token regex"),
                "[REDACTED_TOKEN]",
            ),
            (
                Regex::new(r"(?i)(bearer\s+)[^\s,;]+")
                    .expect("static bearer regex"),
                "$1[REDACTED]",
            ),
            (
                Regex::new(r"(?i)((?:buzz_private_key|buzz_auth_tag|authorization|password|api[_ -]?key|token|secret)\s*[:=]\s*)[^\s,;]+")
                    .expect("static assignment regex"),
                "$1[REDACTED]",
            ),
        ]
    });
    patterns
        .iter()
        .fold(input.to_string(), |current, (regex, replacement)| {
            regex.replace_all(&current, *replacement).to_string()
        })
}

fn error_receipt(request_id: String, error: &OperatorError) -> ReadReceipt {
    ReadReceipt {
        schema_version: SCHEMA_VERSION,
        request_id,
        status: "error".to_string(),
        operation: "messages".to_string(),
        generated_at: chrono::Utc::now().timestamp(),
        relay_host: String::new(),
        identity_pubkey: String::new(),
        requested_limit: 0,
        returned: 0,
        truncated: false,
        events: Vec::new(),
        error_code: Some(error.code.to_string()),
        message: Some(error.message.to_string()),
    }
}

fn ensure_receipt_bound(receipt: &ReadReceipt) -> Result<(), OperatorError> {
    let bytes = serde_json::to_vec(receipt).map_err(|_| {
        OperatorError::new(
            "receipt_invalid",
            "the Buzz read receipt could not be serialized",
        )
    })?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err(OperatorError::new(
            "receipt_oversize",
            "the Buzz read receipt exceeded its output bound",
        ));
    }
    Ok(())
}

fn reject_secret_environment() -> Result<(), OperatorError> {
    for name in ["BUZZ_PRIVATE_KEY", "BUZZ_AUTH_TAG", "NOSTR_PRIVATE_KEY"] {
        if std::env::var_os(name).is_some() {
            return Err(OperatorError::new(
                "secret_input_rejected",
                "the Buzz read helper refuses credential-bearing environment variables",
            ));
        }
    }
    Ok(())
}

fn validate_helper_paths(
    paths: &ClientPaths,
    request: &Path,
    receipt: &Path,
) -> Result<(), OperatorError> {
    paths.ensure()?;
    let request_parent = request
        .parent()
        .and_then(|path| fs::canonicalize(path).ok());
    let receipt_parent = receipt
        .parent()
        .and_then(|path| fs::canonicalize(path).ok());
    let expected_requests = fs::canonicalize(&paths.requests).ok();
    let expected_receipts = fs::canonicalize(&paths.receipts).ok();
    if request_parent != expected_requests || receipt_parent != expected_receipts {
        return Err(OperatorError::new(
            "control_path_rejected",
            "the Buzz read helper paths were outside the owner-only control directory",
        ));
    }
    if receipt.exists() {
        return Err(OperatorError::new(
            "receipt_exists",
            "the Buzz read receipt target already exists",
        ));
    }
    Ok(())
}

fn ensure_owner_only_dir(path: &Path) -> Result<(), OperatorError> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(false).mode(0o700);
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|_| {
                    OperatorError::new(
                        "control_dir_unavailable",
                        "could not create the control directory",
                    )
                })?;
            }
        }
        builder.create(path).map_err(|_| {
            OperatorError::new(
                "control_dir_unavailable",
                "could not create the control directory",
            )
        })?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        OperatorError::new(
            "control_dir_unavailable",
            "could not inspect the control directory",
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(OperatorError::new(
            "control_dir_rejected",
            "the Buzz read control path was not a real directory",
        ));
    }
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != current_uid() {
        return Err(OperatorError::new(
            "control_dir_rejected",
            "the Buzz read control directory had the wrong owner",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            OperatorError::new(
                "control_dir_rejected",
                "the Buzz read control directory was not owner-only",
            )
        })?;
    }
    Ok(())
}

fn write_new_owner_only_json<T: Serialize>(path: &Path, value: &T) -> Result<(), OperatorError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        OperatorError::new(
            "request_invalid",
            "the Buzz read request could not be serialized",
        )
    })?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(OperatorError::new(
            "request_oversize",
            "the Buzz read request exceeded its input bound",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| {
            OperatorError::new(
                "control_file_unavailable",
                "could not create the read request",
            )
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            OperatorError::new(
                "control_file_unavailable",
                "could not persist the read request",
            )
        })
}

fn read_bounded_owner_only_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, OperatorError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| {
            OperatorError::new(
                "control_file_unavailable",
                "could not open an owner-only control file",
            )
        })?;
    let metadata = file.metadata().map_err(|_| {
        OperatorError::new(
            "control_file_unavailable",
            "could not inspect an owner-only control file",
        )
    })?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > max_bytes
    {
        return Err(OperatorError::new(
            "control_file_rejected",
            "the Buzz read control file failed ownership, mode, type, or size checks",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            OperatorError::new(
                "control_file_unavailable",
                "could not read the control file",
            )
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(OperatorError::new(
            "control_file_rejected",
            "the Buzz read control file exceeded its size bound",
        ));
    }
    Ok(bytes)
}

fn write_receipt_atomic(path: &Path, receipt: &ReadReceipt) -> Result<(), OperatorError> {
    ensure_receipt_bound(receipt)?;
    let bytes = serde_json::to_vec(receipt).map_err(|_| {
        OperatorError::new(
            "receipt_invalid",
            "the Buzz read receipt could not be serialized",
        )
    })?;
    let temp_path = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .map_err(|_| {
            OperatorError::new(
                "receipt_unavailable",
                "could not create the Buzz read receipt",
            )
        })?;
    let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    if write_result.is_err() || fs::rename(&temp_path, path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(OperatorError::new(
            "receipt_unavailable",
            "could not persist the Buzz read receipt",
        ));
    }
    Ok(())
}

fn write_launch_agent(
    path: &Path,
    label: &str,
    helper: &Path,
    request: &Path,
    receipt: &Path,
    working_directory: &Path,
) -> Result<(), OperatorError> {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>{}</string><string>{}</string><string>{}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>RunAtLoad</key><true/>\n<key>LaunchOnlyOnce</key><true/>\n<key>ProcessType</key><string>Background</string>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n",
        xml_escape(label),
        xml_escape(&helper.to_string_lossy()),
        HELPER_FLAG,
        xml_escape(&request.to_string_lossy()),
        xml_escape(&receipt.to_string_lossy()),
        xml_escape(&working_directory.to_string_lossy()),
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| {
            OperatorError::new(
                "gui_bridge_unavailable",
                "could not create the GUI bridge request",
            )
        })?;
    file.write_all(xml.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            OperatorError::new(
                "gui_bridge_unavailable",
                "could not persist the GUI bridge request",
            )
        })
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn wait_for_receipt(path: &Path) -> Result<(), OperatorError> {
    let deadline = Instant::now() + HELPER_TIMEOUT;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(OperatorError::new(
        "helper_timeout",
        "the Buzz read helper did not return a receipt before the timeout",
    ))
}

fn prune_old_receipts(directory: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten().take(500) {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.modified().is_ok_and(|modified| modified < cutoff)
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Kind, Tag};

    fn request() -> ReadRequest {
        ReadRequest {
            schema_version: SCHEMA_VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            operation: "messages".to_string(),
            relay: "wss://buildcontext.communities.buzz.xyz".to_string(),
            since: 1_787_872_400,
            until: 1_787_958_800,
            limit: 20,
            excerpt_chars: 280,
            channel: None,
            search: None,
        }
    }

    #[test]
    fn request_rejects_writes_unknown_fields_and_oversize_values() {
        let mut value = serde_json::to_value(request()).unwrap();
        value["operation"] = serde_json::json!("send");
        let parsed: ReadRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            validate_request(&parsed).unwrap_err().code,
            "operation_rejected"
        );

        let mut value = serde_json::to_value(request()).unwrap();
        value["content"] = serde_json::json!("write this");
        assert!(serde_json::from_value::<ReadRequest>(value).is_err());

        let mut oversized = request();
        oversized.limit = MAX_RESULTS + 1;
        assert_eq!(
            validate_request(&oversized).unwrap_err().code,
            "limit_rejected"
        );
        oversized.limit = 1;
        oversized.search = Some("x".repeat(MAX_SEARCH_CHARS + 1));
        assert_eq!(
            validate_request(&oversized).unwrap_err().code,
            "search_rejected"
        );
    }

    #[test]
    fn request_rejects_credentials_and_unbounded_relays() {
        let mut candidate = request();
        candidate.relay = "https://user:pass@example.com".to_string();
        assert_eq!(
            validate_request(&candidate).unwrap_err().code,
            "relay_rejected"
        );
        candidate.relay = "http://127.0.0.1:3000".to_string();
        assert_eq!(
            validate_request(&candidate).unwrap_err().code,
            "relay_rejected"
        );
    }

    #[test]
    fn projection_bounds_and_redacts_secret_material() {
        let channel = "123e4567-e89b-12d3-a456-426614174000";
        let content = format!(
            "work complete BUZZ_PRIVATE_KEY={} Authorization=Bearer-abc secret=shh {}",
            "nsec1".to_string() + &"q".repeat(80),
            "x".repeat(800)
        );
        let event = EventBuilder::new(Kind::Custom(40002), content)
            .tags([Tag::parse(["h", channel]).unwrap()])
            .sign_with_keys(&Keys::generate())
            .unwrap();
        let projected = project_event(&event, 120, &HashMap::new());
        let excerpt = projected.excerpt.unwrap();
        assert!(excerpt.chars().count() <= 121);
        assert!(!excerpt.contains("nsec1"));
        assert!(!excerpt.contains("Bearer-abc"));
        assert!(!excerpt.contains("secret=shh"));
        assert_eq!(projected.channel.as_deref(), Some(channel));
    }

    #[test]
    fn owner_only_control_file_enforces_mode_type_and_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("request.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"{}").unwrap();
        drop(file);
        assert_eq!(read_bounded_owner_only_file(&path, 2).unwrap(), b"{}");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_bounded_owner_only_file(&path, 2).unwrap_err().code,
            "control_file_rejected"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_bounded_owner_only_file(&path, 1).unwrap_err().code,
            "control_file_rejected"
        );
    }

    #[test]
    fn helper_paths_must_stay_in_owner_only_control_tree() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ClientPaths {
            root: temp.path().join("root"),
            requests: temp.path().join("root/requests"),
            receipts: temp.path().join("root/receipts"),
            launch_agents: temp.path().join("root/launch-agents"),
        };
        paths.ensure().unwrap();
        let request_path = paths.requests.join("request.json");
        let outside = temp.path().join("outside.json");
        assert_eq!(
            validate_helper_paths(&paths, &request_path, &outside)
                .unwrap_err()
                .code,
            "control_path_rejected"
        );
    }

    #[test]
    fn receipt_bound_is_enforced() {
        let mut receipt = error_receipt("id".to_string(), &OperatorError::new("x", "y"));
        receipt.events = (0..MAX_RESULTS)
            .map(|index| ReceiptEvent {
                id: format!("{index:064x}"),
                author_pubkey: "a".repeat(64),
                author_name: Some("name".to_string()),
                kind: 40002,
                created_at: index as i64,
                channel: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
                excerpt: Some("x".repeat(MAX_EXCERPT_CHARS as usize)),
            })
            .collect();
        assert!(ensure_receipt_bound(&receipt).is_ok());
        receipt.events.push(ReceiptEvent {
            id: "z".repeat(MAX_RECEIPT_BYTES),
            author_pubkey: String::new(),
            author_name: None,
            kind: 9,
            created_at: 0,
            channel: None,
            excerpt: None,
        });
        assert_eq!(
            ensure_receipt_bound(&receipt).unwrap_err().code,
            "receipt_oversize"
        );
    }
}
