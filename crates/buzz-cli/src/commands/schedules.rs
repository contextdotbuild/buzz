//! Bot-owned durable follow-through schedules backed by private NIP-AE memory.
//!
//! Each schedule occupies one `mem/buzz-follow-through/<id>` slug in the
//! calling agent's agent↔owner namespace. The CLI owns the JSON schema and
//! lifecycle so agents never hand-edit state or reconstruct transitions from
//! prose. Heartbeats claim due schedules before acting; completed schedules are
//! never returned again, and abandoned claims become eligible only after their
//! lease expires.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

use crate::client::BuzzClient;
use crate::commands::mem::{
    get_stored_memory, list_stored_memories, put_stored_memory, ExpectedMemoryHead, StoredMemory,
};
use crate::error::CliError;
use crate::{ScheduleDecisionArg, ScheduleStatusArg, SchedulesCmd};
use buzz_core::engram::{self, Body};
use buzz_core::kind::{KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2};

const LEGACY_SCHEMA_VERSION: u8 = 1;
const TASK_SCHEMA_VERSION: u8 = 2;
const SLUG_PREFIX: &str = "mem/buzz-follow-through/";
const ARCHIVE_SLUG_PREFIX: &str = "mem/buzz-follow-through-archive/";
const BINDING_REGISTRY_SLUG: &str = "mem/buzz-follow-through-bindings";
#[cfg(test)]
const DEFAULT_LEASE_SECONDS: i64 = 30 * 60;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_RECEIPT_BYTES: usize = 256;
const ACTIVE_HEAD_ROLLOVER_BYTES: usize = 48_000;
const MIN_NEXT_CHECK_SECONDS: i64 = 10 * 60;
const MAX_NEXT_CHECK_SECONDS: i64 = 15 * 60;
const MAX_KEEP_MATERIAL_AGE_SECONDS: i64 = 15 * 60;
const TASK_STATE_PREFIX: &str = "buzz-follow-through:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScheduleStatus {
    Pending,
    Claimed,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Claim {
    token: String,
    claimed_at: String,
    lease_expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransitionKind {
    Bound,
    Rescheduled,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScheduleDecision {
    Keep,
    Wake,
    Redirect,
    Completed,
}

impl ScheduleDecision {
    fn task_state(self) -> &'static str {
        match self {
            Self::Keep => "kept",
            Self::Wake => "woken",
            Self::Redirect => "redirected",
            Self::Completed => "completed",
        }
    }
}

impl From<ScheduleDecisionArg> for ScheduleDecision {
    fn from(value: ScheduleDecisionArg) -> Self {
        match value {
            ScheduleDecisionArg::Keep => Self::Keep,
            ScheduleDecisionArg::Wake => Self::Wake,
            ScheduleDecisionArg::Redirect => Self::Redirect,
            ScheduleDecisionArg::Complete => Self::Completed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LastTransition {
    kind: TransitionKind,
    claim_token: String,
    at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskBinding {
    assignee_pubkey: String,
    delegation_event_id: String,
    expected_result: String,
    evidence_locator: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MaterialCheckpoint {
    receipt: String,
    material_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FollowThroughPhase {
    Monitoring,
    SameOwnerWoken,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DecisionAudit {
    claim_token: String,
    decision: ScheduleDecision,
    at: String,
    assignee_pubkey: String,
    delegation_event_id: String,
    receipt: String,
    material_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_due_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replacement_pubkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replacement_delegation_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingAction {
    prepared_claim_token: String,
    decision: ScheduleDecision,
    prepared_at: String,
    receipt: String,
    material_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_due_at: Option<String>,
    assignee_pubkey: String,
    delegation_event_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    replacement_pubkey: Option<String>,
    event: nostr::Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyScheduleV1 {
    schema: u8,
    id: String,
    due_at: String,
    channel_id: String,
    thread_id: String,
    expected_cause: String,
    action: String,
    check: String,
    status: ScheduleStatus,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_transition: Option<LastTransition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AuditArchiveRef {
    sequence: u32,
    slug: String,
    revision: String,
    entry_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditArchive {
    schema: u8,
    schedule_id: String,
    sequence: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<AuditArchiveRef>,
    entries: Vec<DecisionAudit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBindingRegistry {
    schema: u8,
    by_delegation: BTreeMap<String, String>,
    by_schedule: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    schema: u8,
    id: String,
    due_at: String,
    channel_id: String,
    thread_id: String,
    task: Option<TaskBinding>,
    checkpoint: Option<MaterialCheckpoint>,
    phase: Option<FollowThroughPhase>,
    audit: Vec<DecisionAudit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_archive: Option<AuditArchiveRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_action: Option<PendingAction>,
    expected_cause: String,
    action: String,
    check: String,
    status: ScheduleStatus,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_transition: Option<LastTransition>,
}

#[derive(Serialize)]
struct ScheduleOutput<'a> {
    #[serde(flatten)]
    schedule: &'a Schedule,
    revision: &'a str,
    idempotent: bool,
}

struct LoadedSchedule {
    schedule: Schedule,
    revision: String,
    slug: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AssignedTaskStatus {
    Assigned,
    Woken,
    Redirected,
    Completed,
}

impl AssignedTaskStatus {
    fn is_closed(self) -> bool {
        matches!(self, Self::Redirected | Self::Completed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AssignedTaskOutput {
    delegation_event_id: String,
    driver_pubkey: String,
    channel_id: String,
    thread_id: String,
    expected_result: String,
    evidence_locator: String,
    delegated_at: u64,
    status: AssignedTaskStatus,
    updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_event_id: Option<String>,
}

impl From<LegacyScheduleV1> for Schedule {
    fn from(value: LegacyScheduleV1) -> Self {
        Self {
            schema: LEGACY_SCHEMA_VERSION,
            id: value.id,
            due_at: value.due_at,
            channel_id: value.channel_id,
            thread_id: value.thread_id,
            task: None,
            checkpoint: None,
            phase: None,
            audit: Vec::new(),
            audit_archive: None,
            pending_action: None,
            expected_cause: value.expected_cause,
            action: value.action,
            check: value.check,
            status: value.status,
            created_at: value.created_at,
            updated_at: value.updated_at,
            claim: value.claim,
            last_transition: value.last_transition,
        }
    }
}

impl TryFrom<&Schedule> for LegacyScheduleV1 {
    type Error = CliError;

    fn try_from(value: &Schedule) -> Result<Self, Self::Error> {
        if value.schema != LEGACY_SCHEMA_VERSION
            || value.task.is_some()
            || value.checkpoint.is_some()
            || value.phase.is_some()
            || !value.audit.is_empty()
            || value.audit_archive.is_some()
            || value.pending_action.is_some()
        {
            return Err(CliError::Other(format!(
                "schedule `{}` cannot be serialized as legacy schema 1",
                value.id
            )));
        }
        Ok(Self {
            schema: LEGACY_SCHEMA_VERSION,
            id: value.id.clone(),
            due_at: value.due_at.clone(),
            channel_id: value.channel_id.clone(),
            thread_id: value.thread_id.clone(),
            expected_cause: value.expected_cause.clone(),
            action: value.action.clone(),
            check: value.check.clone(),
            status: value.status,
            created_at: value.created_at.clone(),
            updated_at: value.updated_at.clone(),
            claim: value.claim.clone(),
            last_transition: value.last_transition.clone(),
        })
    }
}

fn canonical_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(raw: &str, field: &str) -> Result<DateTime<Utc>, CliError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            CliError::Usage(format!(
                "--{field} must be an RFC3339 timestamp such as 2026-08-26T16:00:00Z: {error}"
            ))
        })
}

fn validate_lower_hex(raw: &str, length: usize) -> bool {
    raw.len() == length
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_next_due_at(now: DateTime<Utc>, raw: &str) -> Result<String, CliError> {
    let due_at = parse_time(raw, "due-at")?;
    let delta = due_at.signed_duration_since(now).num_seconds();
    if !(MIN_NEXT_CHECK_SECONDS..=MAX_NEXT_CHECK_SECONDS).contains(&delta) {
        return Err(CliError::Usage(format!(
            "--due-at must be 10 to 15 minutes after the decision so it is due by the next 15-minute heartbeat (got {delta} seconds)"
        )));
    }
    Ok(canonical_time(due_at))
}

fn validate_id(raw: &str) -> Result<String, CliError> {
    if raw.is_empty() || raw.len() > 64 {
        return Err(CliError::Usage(
            "--id must contain 1..=64 lowercase letters, digits, `_`, or `-`".into(),
        ));
    }
    let mut bytes = raw.bytes();
    let first = bytes.next().unwrap_or_default();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(CliError::Usage(
            "--id must start with a lowercase letter or digit".into(),
        ));
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) {
        return Err(CliError::Usage(
            "--id may contain only lowercase letters, digits, `_`, or `-`".into(),
        ));
    }
    Ok(raw.to_owned())
}

fn validate_text(raw: &str, field: &str) -> Result<String, CliError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(CliError::Usage(format!("--{field} cannot be empty")));
    }
    if value.len() > MAX_TEXT_BYTES {
        return Err(CliError::Usage(format!(
            "--{field} exceeds the {MAX_TEXT_BYTES}-byte limit"
        )));
    }
    if value.chars().any(|character| character == '\0') {
        return Err(CliError::Usage(format!("--{field} cannot contain NUL")));
    }
    Ok(value.to_owned())
}

fn validate_task_text(raw: &str, field: &str) -> Result<String, CliError> {
    let value = validate_text(raw, field)?;
    if value.contains(['\r', '\n']) {
        return Err(CliError::Usage(format!(
            "--{field} must be a single line so its exact delegation marker is unambiguous"
        )));
    }
    Ok(value)
}

fn validate_thread_id(raw: &str) -> Result<String, CliError> {
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CliError::Usage(
            "--thread must be a 64-character hexadecimal Buzz event ID".into(),
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

fn validate_pubkey(raw: &str, field: &str) -> Result<String, CliError> {
    nostr::PublicKey::from_hex(raw)
        .map(|pubkey| pubkey.to_hex())
        .map_err(|_| CliError::Usage(format!("--{field} must be a 64-character hex pubkey")))
}

fn validate_receipt(raw: &str) -> Result<String, CliError> {
    let value = validate_text(raw, "receipt")?;
    if value.len() > MAX_RECEIPT_BYTES {
        return Err(CliError::Usage(format!(
            "--receipt exceeds the {MAX_RECEIPT_BYTES}-byte limit"
        )));
    }
    let Some((kind, revision)) = value.split_once(':') else {
        return Err(CliError::Usage(
            "--receipt must use <kind>:<revision> with a task-bound receipt kind".into(),
        ));
    };
    let valid_lower_hex = |raw: &str, lengths: &[usize]| {
        lengths.contains(&raw.len())
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    match kind {
        "buzz-event" if valid_lower_hex(revision, &[64]) => {}
        "source-event" if valid_lower_hex(revision, &[64]) => {}
        "codex-turn" | "cursor-turn" => {
            let parsed = Uuid::parse_str(revision).map_err(|_| {
                CliError::Usage("--receipt codex-turn/cursor-turn revisions must be UUIDs".into())
            })?;
            return Ok(format!("{kind}:{}", parsed.hyphenated()));
        }
        "git-commit" | "pr-head" if valid_lower_hex(revision, &[40, 64]) => {}
        "document-hash" | "worktree-fingerprint" if valid_lower_hex(revision, &[64]) => {}
        "external-job" if validate_external_job_revision(revision) => {}
        _ => {
            return Err(CliError::Usage(
                "--receipt must contain a canonical immutable revision: buzz-event/source-event use 64 lowercase hex; codex-turn/cursor-turn use UUIDs; git-commit/pr-head use 40 or 64 lowercase hex; document-hash/worktree-fingerprint use 64 lowercase hex; external-job uses provider/job-id@revision"
                    .into(),
                ));
        }
    }
    Ok(value)
}

fn validate_external_job_revision(revision: &str) -> bool {
    let Some((job, immutable_revision)) = revision.split_once('@') else {
        return false;
    };
    let Some((provider, job_id)) = job.split_once('/') else {
        return false;
    };
    let valid_component = |value: &str, min: usize, max: usize| {
        (min..=max).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !valid_component(provider, 2, 32)
        || !valid_component(job_id, 3, 96)
        || !valid_component(immutable_revision, 8, 96)
    {
        return false;
    }
    const PRESENCE_WORDS: &[&str] = &[
        "active", "alive", "busy", "idle", "online", "pending", "queued", "running", "working",
    ];
    !revision
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| PRESENCE_WORDS.contains(&part.to_ascii_lowercase().as_str()))
}

fn schedule_json(schedule: &Schedule) -> Result<String, CliError> {
    let value = if schedule.schema == LEGACY_SCHEMA_VERSION {
        serde_json::to_string(&LegacyScheduleV1::try_from(schedule)?)
    } else if schedule.schema == TASK_SCHEMA_VERSION {
        serde_json::to_string(schedule)
    } else {
        return Err(CliError::Other(format!(
            "schedule `{}` uses unsupported schema {}",
            schedule.id, schedule.schema
        )));
    }
    .map_err(|error| CliError::Other(format!("schedule serialization failed: {error}")))?;
    Ok(value)
}

fn engram_plaintext_len(slug: &str, value: String) -> usize {
    Body::Memory {
        slug: slug.to_owned(),
        value: Some(value),
    }
    .to_json_bytes()
    .len()
}

fn serialized_schedule(schedule: &Schedule) -> Result<String, CliError> {
    let value = schedule_json(schedule)?;
    let plaintext_len = engram_plaintext_len(&schedule_slug(&schedule.id), value.clone());
    if plaintext_len > engram::NIP44_PLAINTEXT_MAX {
        return Err(CliError::Conflict(format!(
            "schedule `{}` requires {plaintext_len} NIP-AE plaintext bytes, above the {}-byte limit",
            schedule.id,
            engram::NIP44_PLAINTEXT_MAX,
        )));
    }
    Ok(value)
}

fn schedule_slug(id: &str) -> String {
    format!("{SLUG_PREFIX}{id}")
}

fn sha256_hex(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn archive_slug(id: &str, sequence: u32, content_hash: &str) -> String {
    format!("{ARCHIVE_SLUG_PREFIX}{id}/{sequence:08}/{content_hash}")
}

fn valid_archive_slug(reference: &AuditArchiveRef, schedule_id: &str) -> bool {
    let prefix = format!(
        "{ARCHIVE_SLUG_PREFIX}{schedule_id}/{:08}/",
        reference.sequence
    );
    reference
        .slug
        .strip_prefix(&prefix)
        .is_some_and(|digest| validate_lower_hex(digest, 64))
}

async fn fetch_event(client: &BuzzClient, event_id: &str) -> Result<nostr::Event, CliError> {
    let filter = serde_json::json!({ "ids": [event_id], "limit": 1 });
    let raw = client.query(&filter).await?;
    let events: Vec<nostr::Event> = serde_json::from_str(&raw)
        .map_err(|error| CliError::Other(format!("failed to parse event query: {error}")))?;
    let event = events
        .into_iter()
        .find(|event| event.id.to_hex() == event_id)
        .ok_or_else(|| CliError::NotFound(format!("delegation event {event_id} not found")))?;
    event
        .verify()
        .map_err(|error| CliError::Other(format!("delegation event is invalid: {error}")))?;
    Ok(event)
}

fn validate_delegation_event(
    event: &nostr::Event,
    driver: &nostr::PublicKey,
    channel_id: &str,
    thread_id: &str,
    task: &TaskBinding,
) -> Result<(), CliError> {
    let kind = event.kind.as_u16() as u32;
    if kind != KIND_STREAM_MESSAGE && kind != KIND_STREAM_MESSAGE_V2 {
        return Err(CliError::Usage(
            "delegation event must be a Buzz stream message".into(),
        ));
    }
    if &event.pubkey != driver {
        return Err(CliError::Usage(
            "delegation event must be authored by the agent creating or redirecting the schedule"
                .into(),
        ));
    }
    let channels: Vec<&str> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
        .filter_map(|tag| tag.content())
        .collect();
    if channels.as_slice() != [channel_id] {
        return Err(CliError::Usage(
            "delegation event must carry exactly the schedule's channel h-tag".into(),
        ));
    }
    let event_id = event.id.to_hex();
    let event_root = buzz_core::nip10::parse_thread_markers(&event.tags)
        .resolve()
        .map(|(root, _)| root);
    if event_id != thread_id && event_root.as_deref() != Some(thread_id) {
        return Err(CliError::Usage(
            "delegation event must be the schedule thread root or a reply in that exact thread"
                .into(),
        ));
    }
    let mut assignees: Vec<String> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("p"))
        .filter_map(|tag| tag.content())
        .map(str::to_ascii_lowercase)
        .collect();
    assignees.sort();
    assignees.dedup();
    if assignees.as_slice() != [task.assignee_pubkey.as_str()] {
        return Err(CliError::Usage(
            "delegation event must p-tag exactly one assignee: the task-bound agent".into(),
        ));
    }
    let expected_marker = format!("Expected result: {}", task.expected_result);
    let evidence_marker = format!("Evidence locator: {}", task.evidence_locator);
    let expected_lines: Vec<&str> = event
        .content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("Expected result:"))
        .collect();
    let evidence_lines: Vec<&str> = event
        .content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("Evidence locator:"))
        .collect();
    if expected_lines.as_slice() != [expected_marker.as_str()]
        || evidence_lines.as_slice() != [evidence_marker.as_str()]
    {
        return Err(CliError::Usage(format!(
            "delegation event must contain exactly one `{expected_marker}` line and exactly one `{evidence_marker}` line"
        )));
    }
    Ok(())
}

fn tag_values<'a>(event: &'a nostr::Event, key: &str) -> Vec<&'a str> {
    event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(key))
        .filter_map(|tag| tag.content())
        .collect()
}

fn single_task_marker(content: &str, marker: &str) -> Option<String> {
    let values: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix(marker).map(str::trim))
        .filter(|value| !value.is_empty())
        .collect();
    (values.len() == 1).then(|| values[0].to_owned())
}

fn event_thread_root(event: &nostr::Event) -> Option<String> {
    let event_id = event.id.to_hex();
    let has_thread_tag = event
        .tags
        .iter()
        .any(|tag| tag.as_slice().first().map(String::as_str) == Some("e"));
    match buzz_core::nip10::parse_thread_markers(&event.tags).resolve() {
        Some((root, _)) => Some(root),
        None if !has_thread_tag => Some(event_id),
        None => None,
    }
}

fn parse_assigned_task_event(
    event: &nostr::Event,
    assignee_pubkey: &str,
) -> Option<AssignedTaskOutput> {
    let kind = event.kind.as_u16() as u32;
    if kind != KIND_STREAM_MESSAGE && kind != KIND_STREAM_MESSAGE_V2 {
        return None;
    }
    let channels = tag_values(event, "h");
    if channels.len() != 1 || Uuid::parse_str(channels[0]).is_err() {
        return None;
    }
    let mut assignees: Vec<String> = tag_values(event, "p")
        .into_iter()
        .map(str::to_ascii_lowercase)
        .collect();
    assignees.sort();
    assignees.dedup();
    if assignees.as_slice() != [assignee_pubkey] {
        return None;
    }
    let expected_result = single_task_marker(&event.content, "Expected result:")?;
    let evidence_locator = single_task_marker(&event.content, "Evidence locator:")?;
    let thread_id = event_thread_root(event)?;
    let delegated_at = event.created_at.as_secs();
    Some(AssignedTaskOutput {
        delegation_event_id: event.id.to_hex(),
        driver_pubkey: event.pubkey.to_hex(),
        channel_id: channels[0].to_owned(),
        thread_id,
        expected_result,
        evidence_locator,
        delegated_at,
        status: AssignedTaskStatus::Assigned,
        updated_at: delegated_at,
        status_event_id: None,
    })
}

fn task_state_reference(event: &nostr::Event) -> Option<&str> {
    let values = tag_values(event, "d");
    if values.len() != 1 {
        return None;
    }
    values[0].strip_prefix(TASK_STATE_PREFIX)
}

fn apply_task_state(task: &mut AssignedTaskOutput, event: &nostr::Event) {
    if event.pubkey.to_hex() != task.driver_pubkey
        || event.created_at.as_secs() < task.delegated_at
        || tag_values(event, "h").as_slice() != [task.channel_id.as_str()]
        || event_thread_root(event).as_deref() != Some(task.thread_id.as_str())
    {
        return;
    }
    let states = tag_values(event, "task-status");
    let status = match states.as_slice() {
        ["woken"] => AssignedTaskStatus::Woken,
        ["redirected"] => AssignedTaskStatus::Redirected,
        ["completed"] => AssignedTaskStatus::Completed,
        _ => return,
    };
    let updated_at = event.created_at.as_secs();
    let event_id = event.id.to_hex();
    if (updated_at, event_id.as_str())
        <= (
            task.updated_at,
            task.status_event_id.as_deref().unwrap_or(""),
        )
    {
        return;
    }
    task.status = status;
    task.updated_at = updated_at;
    task.status_event_id = Some(event_id);
}

async fn assigned(
    client: &BuzzClient,
    include_closed: bool,
    since: Option<i64>,
    limit: u32,
) -> Result<(), CliError> {
    if limit == 0 || limit > 500 {
        return Err(CliError::Usage("--limit must be between 1 and 500".into()));
    }
    if since.is_some_and(|value| value < 0) {
        return Err(CliError::Usage(
            "--since must be a non-negative Unix timestamp".into(),
        ));
    }
    let assignee = client.keys().public_key().to_hex();
    let mut filter = serde_json::json!({
        "kinds": [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2],
        "#p": [assignee],
        "limit": limit,
    });
    if let Some(since) = since {
        filter["since"] = serde_json::json!(since);
    }
    let raw = client.query(&filter).await?;
    let events: Vec<nostr::Event> = serde_json::from_str(&raw)
        .map_err(|error| CliError::Other(format!("failed to parse assignment query: {error}")))?;
    let mut tasks: Vec<AssignedTaskOutput> = events
        .iter()
        .filter(|event| event.verify().is_ok())
        .filter_map(|event| parse_assigned_task_event(event, &assignee))
        .collect();

    if !tasks.is_empty() {
        let references: Vec<String> = tasks
            .iter()
            .map(|task| format!("{TASK_STATE_PREFIX}{}", task.delegation_event_id))
            .collect();
        let state_limit = u32::try_from(tasks.len().saturating_mul(8).min(5_000)).unwrap_or(5_000);
        let state_filter = serde_json::json!({
            "kinds": [KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2],
            "#d": references,
            "limit": state_limit,
        });
        let raw = client.query(&state_filter).await?;
        let state_events: Vec<nostr::Event> = serde_json::from_str(&raw).map_err(|error| {
            CliError::Other(format!("failed to parse assignment-state query: {error}"))
        })?;
        let task_indexes: HashMap<String, usize> = tasks
            .iter()
            .enumerate()
            .map(|(index, task)| (task.delegation_event_id.clone(), index))
            .collect();
        for event in state_events.iter().filter(|event| event.verify().is_ok()) {
            let Some(reference) = task_state_reference(event) else {
                continue;
            };
            let Some(index) = task_indexes.get(reference).copied() else {
                continue;
            };
            apply_task_state(&mut tasks[index], event);
        }
    }

    if !include_closed {
        tasks.retain(|task| !task.status.is_closed());
    }
    tasks.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.delegation_event_id.cmp(&a.delegation_event_id))
    });
    println!(
        "{}",
        serde_json::to_string(&tasks)
            .map_err(|error| CliError::Other(format!("assigned-work output failed: {error}")))?
    );
    Ok(())
}

async fn fetch_and_validate_delegation(
    client: &BuzzClient,
    event_id: &str,
    channel_id: &str,
    thread_id: &str,
    task: &TaskBinding,
) -> Result<(), CliError> {
    let event = fetch_event(client, event_id).await?;
    validate_delegation_event(
        &event,
        &client.keys().public_key(),
        channel_id,
        thread_id,
        task,
    )
}

fn validate_adopted_source_event(
    event: &nostr::Event,
    assignee: &nostr::PublicKey,
) -> Result<(String, String), CliError> {
    let kind = event.kind.as_u16() as u32;
    if kind != KIND_STREAM_MESSAGE && kind != KIND_STREAM_MESSAGE_V2 {
        return Err(CliError::Usage(
            "adopted source must be a Buzz stream message".into(),
        ));
    }
    if event.content.trim().is_empty() {
        return Err(CliError::Usage(
            "adopted source must contain a nonempty obligation".into(),
        ));
    }
    let channels = tag_values(event, "h");
    if channels.len() != 1 || Uuid::parse_str(channels[0]).is_err() {
        return Err(CliError::Usage(
            "adopted source must carry exactly one valid channel h-tag".into(),
        ));
    }
    let assignee = assignee.to_hex();
    if !tag_values(event, "p")
        .iter()
        .any(|pubkey| pubkey.eq_ignore_ascii_case(&assignee))
    {
        return Err(CliError::Usage(
            "adopted source must be addressed to this identity".into(),
        ));
    }
    let thread_id = event_thread_root(event).ok_or_else(|| {
        CliError::Usage("adopted source must belong to one unambiguous Buzz thread".into())
    })?;
    Ok((channels[0].to_owned(), thread_id))
}

fn adopted_schedule_id(event_id: &str) -> Result<String, CliError> {
    let event_id = validate_thread_id(event_id)?;
    Ok(format!("adopt-{}", &event_id[..40]))
}

async fn adopt(
    client: &BuzzClient,
    source_event: &str,
    due_at: &str,
    expected_result: &str,
    evidence_locator: &str,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let source_event = validate_thread_id(source_event)?;
    let event = fetch_event(client, &source_event).await?;
    let (channel_id, thread_id) =
        validate_adopted_source_event(&event, &client.keys().public_key())?;
    let id = adopted_schedule_id(&source_event)?;
    let material_at = chrono::DateTime::<Utc>::from_timestamp(event.created_at.as_secs() as i64, 0)
        .ok_or_else(|| CliError::Other("adopted source timestamp is out of range".into()))?;
    let task = TaskBinding {
        assignee_pubkey: client.keys().public_key().to_hex(),
        delegation_event_id: source_event.clone(),
        expected_result: validate_task_text(expected_result, "expected-result")?,
        evidence_locator: validate_task_text(evidence_locator, "evidence-locator")?,
    };
    let decision_at = Utc::now();
    let due_at = canonical_time(parse_time(due_at, "due-at")?);
    validate_next_due_at(decision_at, &due_at)?;
    let schedule = Schedule {
        schema: TASK_SCHEMA_VERSION,
        id: id.clone(),
        due_at,
        channel_id,
        thread_id,
        task: Some(task),
        checkpoint: Some(MaterialCheckpoint {
            receipt: format!("source-event:{source_event}"),
            material_at: canonical_time(material_at),
        }),
        phase: Some(FollowThroughPhase::Monitoring),
        audit: Vec::new(),
        audit_archive: None,
        pending_action: None,
        expected_cause: "The adopted obligation remains unfinished at its next check".into(),
        action: "Inspect the exact conversation and named evidence; continue the same work or recover it without duplicating an active owner".into(),
        check: "Read the source thread and exact evidence locator; require a newer task-bound material receipt, not generic presence".into(),
        status: ScheduleStatus::Pending,
        created_at: canonical_time(decision_at),
        updated_at: canonical_time(decision_at),
        claim: None,
        last_transition: None,
    };
    validate_schedule(&schedule)?;
    let slug = schedule_slug(&id);
    let (owner_pubkey, existing) = get_stored_memory(client, owner, &slug).await?;
    if let Some(existing) = existing {
        let loaded = parse_stored(existing)?;
        if create_definition_matches(&loaded.schedule, &schedule) {
            return print_one(&loaded.schedule, &loaded.revision, true);
        }
        return Err(CliError::Conflict(format!(
            "adopted obligation `{source_event}` already has different state or instructions"
        )));
    }
    reserve_task_binding(client, &owner_pubkey, &source_event, &id).await?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &slug,
        serialized_schedule(&schedule)?,
        ExpectedMemoryHead::Missing,
    )
    .await?;
    print_one(&schedule, &revision, false)
}

async fn verify_task_receipt(
    client: &BuzzClient,
    channel_id: &str,
    thread_id: &str,
    assignee_pubkey: &str,
    checkpoint: &MaterialCheckpoint,
) -> Result<(), CliError> {
    let Some(event_id) = checkpoint.receipt.strip_prefix("buzz-event:") else {
        return Ok(());
    };
    let event = fetch_event(client, event_id).await?;
    validate_buzz_receipt_event(&event, channel_id, thread_id, assignee_pubkey)?;
    require_buzz_event_material_at(&event, &checkpoint.material_at)?;
    Ok(())
}

async fn reserve_task_binding(
    client: &BuzzClient,
    owner: &nostr::PublicKey,
    delegation_event_id: &str,
    schedule_id: &str,
) -> Result<(), CliError> {
    let owner_hex = owner.to_hex();
    for _ in 0..4 {
        let (_, existing) =
            get_stored_memory(client, Some(&owner_hex), BINDING_REGISTRY_SLUG).await?;
        let mut registry = if let Some(stored) = &existing {
            serde_json::from_str::<TaskBindingRegistry>(&stored.value).map_err(|error| {
                CliError::Other(format!("task binding registry is invalid: {error}"))
            })?
        } else {
            TaskBindingRegistry {
                schema: 1,
                by_delegation: BTreeMap::new(),
                by_schedule: BTreeMap::new(),
            }
        };
        if reserve_binding_in_registry(&mut registry, delegation_event_id, schedule_id)? {
            return Ok(());
        }
        let value = serde_json::to_string(&registry).map_err(|error| {
            CliError::Other(format!(
                "task binding registry serialization failed: {error}"
            ))
        })?;
        let expected_revision = existing.as_ref().map(|stored| stored.event.id.to_hex());
        let expected = expected_revision
            .as_deref()
            .map_or(ExpectedMemoryHead::Missing, ExpectedMemoryHead::Event);
        match put_stored_memory(client, owner, BINDING_REGISTRY_SLUG, value, expected).await {
            Ok(_) => return Ok(()),
            Err(CliError::Conflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CliError::Conflict(
        "task binding registry changed repeatedly; retry from fresh state".into(),
    ))
}

fn reserve_binding_in_registry(
    registry: &mut TaskBindingRegistry,
    delegation_event_id: &str,
    schedule_id: &str,
) -> Result<bool, CliError> {
    validate_binding_registry(registry)?;
    match registry.by_delegation.get(delegation_event_id) {
        Some(existing_schedule)
            if existing_schedule == schedule_id
                && registry.by_schedule.get(schedule_id).map(String::as_str)
                    == Some(delegation_event_id) =>
        {
            return Ok(true);
        }
        Some(_) => {
            return Err(CliError::Conflict(format!(
                "delegation event `{delegation_event_id}` is already bound to another schedule"
            )));
        }
        None => {}
    }
    if let Some(existing_delegation) = registry.by_schedule.get(schedule_id) {
        return Err(CliError::Conflict(format!(
            "schedule `{schedule_id}` is already bound to delegation event `{existing_delegation}`"
        )));
    }
    registry
        .by_delegation
        .insert(delegation_event_id.to_owned(), schedule_id.to_owned());
    registry
        .by_schedule
        .insert(schedule_id.to_owned(), delegation_event_id.to_owned());
    Ok(false)
}

fn validate_binding_registry(registry: &TaskBindingRegistry) -> Result<(), CliError> {
    if registry.schema != 1
        || registry
            .by_schedule
            .iter()
            .any(|(schedule, delegation)| registry.by_delegation.get(delegation) != Some(schedule))
        || registry
            .by_delegation
            .values()
            .any(|schedule| !registry.by_schedule.contains_key(schedule))
    {
        return Err(CliError::Other(
            "task binding registry has inconsistent mappings".into(),
        ));
    }
    Ok(())
}

fn advance_binding_in_registry(
    registry: &mut TaskBindingRegistry,
    schedule_id: &str,
    expected_delegation_event_id: &str,
    replacement_delegation_event_id: &str,
) -> Result<bool, CliError> {
    validate_binding_registry(registry)?;
    let current = registry.by_schedule.get(schedule_id).ok_or_else(|| {
        CliError::Conflict(format!(
            "schedule `{schedule_id}` has no reserved task binding"
        ))
    })?;
    if current == replacement_delegation_event_id
        && registry
            .by_delegation
            .get(replacement_delegation_event_id)
            .map(String::as_str)
            == Some(schedule_id)
    {
        if registry
            .by_delegation
            .get(expected_delegation_event_id)
            .map(String::as_str)
            != Some(schedule_id)
        {
            return Err(CliError::Conflict(format!(
                "schedule `{schedule_id}` lost its historical delegation reservation `{expected_delegation_event_id}`"
            )));
        }
        return Ok(true);
    }
    if current != expected_delegation_event_id {
        return Err(CliError::Conflict(format!(
            "schedule `{schedule_id}` task binding changed from `{expected_delegation_event_id}` to `{current}`"
        )));
    }
    if let Some(existing_schedule) = registry.by_delegation.get(replacement_delegation_event_id) {
        return Err(CliError::Conflict(format!(
            "delegation event `{replacement_delegation_event_id}` is already bound to schedule `{existing_schedule}`"
        )));
    }
    registry.by_delegation.insert(
        replacement_delegation_event_id.to_owned(),
        schedule_id.to_owned(),
    );
    registry.by_schedule.insert(
        schedule_id.to_owned(),
        replacement_delegation_event_id.to_owned(),
    );
    Ok(false)
}

async fn advance_task_binding(
    client: &BuzzClient,
    owner: &nostr::PublicKey,
    schedule_id: &str,
    expected_delegation_event_id: &str,
    replacement_delegation_event_id: &str,
) -> Result<(), CliError> {
    let owner_hex = owner.to_hex();
    for _ in 0..4 {
        let (_, existing) =
            get_stored_memory(client, Some(&owner_hex), BINDING_REGISTRY_SLUG).await?;
        let stored = existing.ok_or_else(|| {
            CliError::Conflict("task binding registry is missing during redirect".into())
        })?;
        let mut registry =
            serde_json::from_str::<TaskBindingRegistry>(&stored.value).map_err(|error| {
                CliError::Other(format!("task binding registry is invalid: {error}"))
            })?;
        if advance_binding_in_registry(
            &mut registry,
            schedule_id,
            expected_delegation_event_id,
            replacement_delegation_event_id,
        )? {
            return Ok(());
        }
        let value = serde_json::to_string(&registry).map_err(|error| {
            CliError::Other(format!(
                "task binding registry serialization failed: {error}"
            ))
        })?;
        match put_stored_memory(
            client,
            owner,
            BINDING_REGISTRY_SLUG,
            value,
            ExpectedMemoryHead::Event(&stored.event.id.to_hex()),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(CliError::Conflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CliError::Conflict(
        "task binding registry changed repeatedly during redirect; retry from fresh state".into(),
    ))
}

fn validate_buzz_receipt_event(
    event: &nostr::Event,
    channel_id: &str,
    thread_id: &str,
    assignee_pubkey: &str,
) -> Result<(), CliError> {
    let kind = event.kind.as_u16() as u32;
    if kind != KIND_STREAM_MESSAGE && kind != KIND_STREAM_MESSAGE_V2 {
        return Err(CliError::Usage(
            "buzz-event receipt must be a signed Buzz stream-message callback".into(),
        ));
    }
    if event.content.trim().is_empty() {
        return Err(CliError::Usage(
            "buzz-event receipt must contain nonempty material callback content".into(),
        ));
    }
    if event.pubkey.to_hex() != assignee_pubkey {
        return Err(CliError::Usage(
            "buzz-event receipt must be authored by the task's exact assignee".into(),
        ));
    }
    let channels: Vec<&str> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
        .filter_map(|tag| tag.content())
        .collect();
    if channels.as_slice() != [channel_id] {
        return Err(CliError::Usage(
            "buzz-event receipt must carry exactly the task channel h-tag".into(),
        ));
    }
    let event_id = event.id.to_hex();
    let root = buzz_core::nip10::parse_thread_markers(&event.tags)
        .resolve()
        .map(|(root, _)| root);
    if event_id != thread_id && root.as_deref() != Some(thread_id) {
        return Err(CliError::Usage(
            "buzz-event receipt must be the task thread root or a reply in that exact thread"
                .into(),
        ));
    }
    Ok(())
}

fn canonical_buzz_event_material_at(event: &nostr::Event) -> Result<String, CliError> {
    let timestamp = i64::try_from(event.created_at.as_secs())
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| CliError::Usage("buzz-event timestamp is outside RFC3339 range".into()))?;
    Ok(canonical_time(timestamp))
}

fn require_buzz_event_material_at(
    event: &nostr::Event,
    supplied: &str,
) -> Result<String, CliError> {
    let event_time = canonical_buzz_event_material_at(event)?;
    if supplied != event_time {
        return Err(CliError::Usage(format!(
            "--material-at must equal the verified buzz-event timestamp {event_time}"
        )));
    }
    Ok(event_time)
}

fn validate_canonical_receipt_field(schedule_id: &str, receipt: &str) -> Result<(), CliError> {
    let canonical = validate_receipt(receipt)?;
    if canonical != receipt {
        return Err(invalid_stored(
            schedule_id,
            "receipt is not in canonical form",
        ));
    }
    Ok(())
}

fn invalid_stored(schedule_id: &str, detail: impl std::fmt::Display) -> CliError {
    CliError::Other(format!(
        "stored follow-through schedule `{schedule_id}` is inconsistent: {detail}"
    ))
}

fn validate_status_claim(schedule: &Schedule) -> Result<(), CliError> {
    match (&schedule.status, &schedule.claim) {
        (ScheduleStatus::Pending, None)
        | (ScheduleStatus::Claimed, Some(_))
        | (ScheduleStatus::Completed, None) => {}
        _ => {
            return Err(invalid_stored(
                &schedule.id,
                "status and claim do not describe the same lifecycle state",
            ));
        }
    }
    if let Some(claim) = &schedule.claim {
        let claimed_at = parse_time(&claim.claimed_at, "claimed-at")?;
        let lease_expires_at = parse_time(&claim.lease_expires_at, "lease-expires-at")?;
        if claim.token.len() != 32
            || !claim.token.bytes().all(|byte| byte.is_ascii_hexdigit())
            || lease_expires_at <= claimed_at
        {
            return Err(invalid_stored(
                &schedule.id,
                "claim shape or lease is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_audit_shape(schedule: &Schedule, entry: &DecisionAudit) -> Result<(), CliError> {
    if !validate_lower_hex(&entry.claim_token, 32) {
        return Err(invalid_stored(
            &schedule.id,
            "audit claim token is not a 32-character lowercase hex value",
        ));
    }
    validate_pubkey(&entry.assignee_pubkey, "assignee")?;
    validate_thread_id(&entry.delegation_event_id)?;
    validate_canonical_receipt_field(&schedule.id, &entry.receipt)?;
    let decision_at = parse_time(&entry.at, "decision-at")?;
    let material_at = parse_time(&entry.material_at, "material-at")?;
    if material_at > decision_at {
        return Err(invalid_stored(
            &schedule.id,
            "audit material timestamp is later than its decision",
        ));
    }
    match entry.decision {
        ScheduleDecision::Keep => {
            if entry.next_due_at.is_none()
                || entry.replacement_pubkey.is_some()
                || entry.replacement_delegation_event_id.is_some()
                || entry.action_event_id.is_some()
                || entry.action_content.is_some()
            {
                return Err(invalid_stored(&schedule.id, "keep audit fields disagree"));
            }
        }
        ScheduleDecision::Wake => {
            if entry.next_due_at.is_none()
                || entry.replacement_pubkey.is_some()
                || entry.replacement_delegation_event_id.is_some()
                || entry.action_event_id.is_none()
                || entry.action_content.is_none()
            {
                return Err(invalid_stored(&schedule.id, "wake audit fields disagree"));
            }
        }
        ScheduleDecision::Redirect => {
            let Some(replacement) = &entry.replacement_pubkey else {
                return Err(invalid_stored(&schedule.id, "redirect has no replacement"));
            };
            let Some(event_id) = &entry.replacement_delegation_event_id else {
                return Err(invalid_stored(
                    &schedule.id,
                    "redirect has no replacement delegation event",
                ));
            };
            validate_pubkey(replacement, "replacement")?;
            validate_thread_id(event_id)?;
            if entry.next_due_at.is_none()
                || replacement == &entry.assignee_pubkey
                || event_id == &entry.delegation_event_id
                || entry.action_event_id.as_deref() != Some(event_id)
                || entry.action_content.is_none()
            {
                return Err(invalid_stored(
                    &schedule.id,
                    "redirect did not change both assignee and delegation event",
                ));
            }
        }
        ScheduleDecision::Completed => {
            if entry.next_due_at.is_some()
                || entry.replacement_pubkey.is_some()
                || entry.replacement_delegation_event_id.is_some()
                || entry.action_event_id.is_none()
                || entry.action_content.is_none()
            {
                return Err(invalid_stored(
                    &schedule.id,
                    "completion audit fields disagree",
                ));
            }
        }
    }
    if let Some(event_id) = &entry.action_event_id {
        validate_thread_id(event_id)?;
    }
    if let Some(content) = &entry.action_content {
        validate_text(content, "message")?;
    }
    if let Some(due_at) = &entry.next_due_at {
        validate_next_due_at(decision_at, due_at)?;
    }
    Ok(())
}

fn validate_pending_action(schedule: &Schedule, pending: &PendingAction) -> Result<(), CliError> {
    if schedule.status != ScheduleStatus::Claimed || schedule.claim.is_none() {
        return Err(invalid_stored(
            &schedule.id,
            "a pending visible action must retain a claimed schedule head",
        ));
    }
    if pending.decision == ScheduleDecision::Keep {
        return Err(invalid_stored(
            &schedule.id,
            "a keep decision cannot create a visible pending action",
        ));
    }
    if !validate_lower_hex(&pending.prepared_claim_token, 32) {
        return Err(invalid_stored(
            &schedule.id,
            "pending action claim token is invalid",
        ));
    }
    validate_canonical_receipt_field(&schedule.id, &pending.receipt)?;
    let prepared_at = parse_time(&pending.prepared_at, "prepared-at")?;
    let material_at = parse_time(&pending.material_at, "material-at")?;
    if material_at > prepared_at {
        return Err(invalid_stored(
            &schedule.id,
            "pending action material timestamp is later than preparation",
        ));
    }
    match pending.decision {
        ScheduleDecision::Wake | ScheduleDecision::Redirect => {
            let due_at = pending.next_due_at.as_deref().ok_or_else(|| {
                invalid_stored(&schedule.id, "pending recovery action has no next due time")
            })?;
            validate_next_due_at(prepared_at, due_at)?;
        }
        ScheduleDecision::Completed => {
            if pending.next_due_at.is_some() {
                return Err(invalid_stored(
                    &schedule.id,
                    "pending completion unexpectedly has a next due time",
                ));
            }
        }
        ScheduleDecision::Keep => {
            return Err(invalid_stored(
                &schedule.id,
                "keep cannot be a pending visible action",
            ));
        }
    }
    validate_pubkey(&pending.assignee_pubkey, "assignee")?;
    validate_thread_id(&pending.delegation_event_id)?;
    let task = schedule.task.as_ref().ok_or_else(|| {
        invalid_stored(&schedule.id, "pending action has no current task binding")
    })?;
    if task.assignee_pubkey != pending.assignee_pubkey
        || task.delegation_event_id != pending.delegation_event_id
    {
        return Err(invalid_stored(
            &schedule.id,
            "pending action does not match the current task binding",
        ));
    }
    pending.event.verify().map_err(|error| {
        invalid_stored(&schedule.id, format!("pending event is invalid: {error}"))
    })?;
    validate_text(&pending.event.content, "message")?;
    if canonical_buzz_event_material_at(&pending.event)? != pending.prepared_at {
        return Err(invalid_stored(
            &schedule.id,
            "pending action event timestamp does not match preparation time",
        ));
    }
    match pending.decision {
        ScheduleDecision::Wake => validate_action_event_scope(
            &pending.event,
            &pending.event.pubkey,
            &schedule.channel_id,
            &schedule.thread_id,
            &[pending.assignee_pubkey.as_str()],
        )?,
        ScheduleDecision::Redirect => {
            let replacement = pending.replacement_pubkey.as_ref().ok_or_else(|| {
                invalid_stored(&schedule.id, "pending redirect has no replacement")
            })?;
            let replacement_task = TaskBinding {
                assignee_pubkey: replacement.clone(),
                delegation_event_id: pending.event.id.to_hex(),
                expected_result: task.expected_result.clone(),
                evidence_locator: task.evidence_locator.clone(),
            };
            validate_delegation_event(
                &pending.event,
                &pending.event.pubkey,
                &schedule.channel_id,
                &schedule.thread_id,
                &replacement_task,
            )?;
        }
        ScheduleDecision::Completed => validate_action_event_scope(
            &pending.event,
            &pending.event.pubkey,
            &schedule.channel_id,
            &schedule.thread_id,
            &[],
        )?,
        ScheduleDecision::Keep => {
            return Err(invalid_stored(&schedule.id, "keep cannot be pending"));
        }
    }
    Ok(())
}

fn validate_schedule(schedule: &Schedule) -> Result<(), CliError> {
    let expected_slug = schedule_slug(&validate_id(&schedule.id)?);
    parse_time(&schedule.due_at, "due-at")?;
    let created_at = parse_time(&schedule.created_at, "created-at")?;
    let updated_at = parse_time(&schedule.updated_at, "updated-at")?;
    if updated_at < created_at {
        return Err(invalid_stored(
            &schedule.id,
            "updated_at predates created_at",
        ));
    }
    Uuid::parse_str(&schedule.channel_id).map_err(|_| {
        CliError::Other(format!(
            "stored schedule `{}` has an invalid channel UUID",
            schedule.id
        ))
    })?;
    validate_thread_id(&schedule.thread_id)?;
    validate_text(&schedule.expected_cause, "expected-cause")?;
    validate_text(&schedule.action, "action")?;
    validate_text(&schedule.check, "check")?;
    validate_status_claim(schedule)?;

    match schedule.schema {
        LEGACY_SCHEMA_VERSION => {
            LegacyScheduleV1::try_from(schedule)?;
        }
        TASK_SCHEMA_VERSION => {
            let task = schedule
                .task
                .as_ref()
                .ok_or_else(|| invalid_stored(&schedule.id, "schema 2 requires a task binding"))?;
            validate_pubkey(&task.assignee_pubkey, "assignee")?;
            validate_thread_id(&task.delegation_event_id)?;
            validate_text(&task.expected_result, "expected-result")?;
            validate_text(&task.evidence_locator, "evidence-locator")?;
            let checkpoint = schedule.checkpoint.as_ref().ok_or_else(|| {
                invalid_stored(&schedule.id, "schema 2 requires a material checkpoint")
            })?;
            validate_canonical_receipt_field(&schedule.id, &checkpoint.receipt)?;
            parse_time(&checkpoint.material_at, "material-at")?;
            let phase = schedule.phase.ok_or_else(|| {
                invalid_stored(&schedule.id, "schema 2 requires a follow-through phase")
            })?;
            if (schedule.status == ScheduleStatus::Completed)
                != (phase == FollowThroughPhase::Completed)
            {
                return Err(invalid_stored(
                    &schedule.id,
                    "completed status and completed phase disagree",
                ));
            }
            if let Some(reference) = &schedule.audit_archive {
                if reference.sequence == 0
                    || !valid_archive_slug(reference, &schedule.id)
                    || !validate_lower_hex(&reference.revision, 64)
                    || reference.entry_count == 0
                {
                    return Err(invalid_stored(
                        &schedule.id,
                        "audit archive reference is invalid",
                    ));
                }
            }
            if let Some(pending) = &schedule.pending_action {
                validate_pending_action(schedule, pending)?;
            }
            let mut expected_assignee: Option<(&str, &str)> = None;
            for entry in &schedule.audit {
                validate_audit_shape(schedule, entry)?;
                if let Some((assignee, delegation)) = expected_assignee {
                    if entry.assignee_pubkey != assignee || entry.delegation_event_id != delegation
                    {
                        return Err(invalid_stored(
                            &schedule.id,
                            "audit assignee chain is discontinuous",
                        ));
                    }
                }
                expected_assignee = if entry.decision == ScheduleDecision::Redirect {
                    Some((
                        entry.replacement_pubkey.as_deref().unwrap_or_default(),
                        entry
                            .replacement_delegation_event_id
                            .as_deref()
                            .unwrap_or_default(),
                    ))
                } else {
                    Some((&entry.assignee_pubkey, &entry.delegation_event_id))
                };
            }
            if let Some((assignee, delegation)) = expected_assignee {
                if task.assignee_pubkey != assignee || task.delegation_event_id != delegation {
                    return Err(invalid_stored(
                        &schedule.id,
                        "current task does not match the audit's final assignee",
                    ));
                }
            }
            if let Some(last) = schedule.audit.last() {
                let checkpoint_matches = checkpoint.receipt == last.receipt
                    && checkpoint.material_at == last.material_at;
                match phase {
                    FollowThroughPhase::Monitoring => {
                        if last.decision != ScheduleDecision::Keep
                            && last.decision != ScheduleDecision::Redirect
                        {
                            return Err(invalid_stored(
                                &schedule.id,
                                "monitoring phase does not follow keep or redirect",
                            ));
                        }
                        if !checkpoint_matches {
                            return Err(invalid_stored(
                                &schedule.id,
                                "monitoring checkpoint does not match the latest decision",
                            ));
                        }
                    }
                    FollowThroughPhase::SameOwnerWoken => {
                        if last.decision != ScheduleDecision::Wake || !checkpoint_matches {
                            return Err(invalid_stored(
                                &schedule.id,
                                "same-owner-woken phase does not match the latest wake",
                            ));
                        }
                    }
                    FollowThroughPhase::Completed => {
                        if last.decision != ScheduleDecision::Completed || !checkpoint_matches {
                            return Err(invalid_stored(
                                &schedule.id,
                                "completed phase does not match the latest completion",
                            ));
                        }
                    }
                }
                let transition = schedule.last_transition.as_ref().ok_or_else(|| {
                    invalid_stored(&schedule.id, "audited schedule has no last transition")
                })?;
                if transition.claim_token != last.claim_token {
                    return Err(invalid_stored(
                        &schedule.id,
                        "last transition does not match the latest audit claim",
                    ));
                }
            } else if phase != FollowThroughPhase::Monitoring {
                return Err(invalid_stored(
                    &schedule.id,
                    "a newly bound task must begin in monitoring phase",
                ));
            }
        }
        _ => {
            return Err(invalid_stored(
                &schedule.id,
                format!("unsupported schema {}", schedule.schema),
            ));
        }
    }
    let _ = expected_slug;
    serialized_schedule(schedule)?;
    Ok(())
}

fn parse_stored(entry: StoredMemory) -> Result<LoadedSchedule, CliError> {
    let raw: serde_json::Value = serde_json::from_str(&entry.value).map_err(|error| {
        CliError::Other(format!(
            "stored follow-through schedule `{}` is invalid JSON: {error}",
            entry.slug
        ))
    })?;
    let schema = raw.get("schema").and_then(serde_json::Value::as_u64);
    let schedule = match schema {
        Some(value) if value == u64::from(LEGACY_SCHEMA_VERSION) => {
            serde_json::from_value::<LegacyScheduleV1>(raw)
                .map(Schedule::from)
                .map_err(|error| {
                    CliError::Other(format!(
                        "stored legacy schedule `{}` has an invalid schema-1 shape: {error}",
                        entry.slug
                    ))
                })?
        }
        Some(value) if value == u64::from(TASK_SCHEMA_VERSION) => {
            serde_json::from_value::<Schedule>(raw).map_err(|error| {
                CliError::Other(format!(
                    "stored task schedule `{}` has an invalid schema-2 shape: {error}",
                    entry.slug
                ))
            })?
        }
        Some(value) => {
            return Err(CliError::Other(format!(
                "stored follow-through schedule `{}` uses unsupported schema {value}",
                entry.slug
            )));
        }
        None => {
            return Err(CliError::Other(format!(
                "stored follow-through schedule `{}` has no schema",
                entry.slug
            )));
        }
    };
    let expected_slug = schedule_slug(&validate_id(&schedule.id)?);
    if entry.slug != expected_slug {
        return Err(CliError::Other(format!(
            "stored follow-through schedule slug `{}` does not match id `{}`",
            entry.slug, schedule.id
        )));
    }
    validate_schedule(&schedule)?;
    Ok(LoadedSchedule {
        schedule,
        revision: entry.event.id.to_hex(),
        slug: entry.slug,
    })
}

async fn load_all(
    client: &BuzzClient,
    owner: Option<&str>,
) -> Result<Vec<LoadedSchedule>, CliError> {
    let mut schedules: Vec<LoadedSchedule> = list_stored_memories(client, owner, SLUG_PREFIX)
        .await?
        .into_iter()
        .map(parse_stored)
        .collect::<Result<_, _>>()?;

    // Broad encrypted-memory listings are the only discovery path for legacy
    // schema-1 schedules, but they can temporarily omit a recently written
    // head. Every task-bound schema-2 schedule is also recorded in the exact
    // binding registry, so use that registry as a bounded fallback and fetch
    // only registered schedule IDs that the broad listing did not return.
    let (_, stored_registry) = get_stored_memory(client, owner, BINDING_REGISTRY_SLUG).await?;
    let Some(stored_registry) = stored_registry else {
        return Ok(schedules);
    };
    let registry = serde_json::from_str::<TaskBindingRegistry>(&stored_registry.value)
        .map_err(|error| CliError::Other(format!("task binding registry is invalid: {error}")))?;
    let present: HashSet<String> = schedules
        .iter()
        .map(|loaded| loaded.schedule.id.clone())
        .collect();
    for id in missing_registered_schedule_ids(&registry, &present)? {
        let slug = schedule_slug(&id);
        let (_, stored) = get_stored_memory(client, owner, &slug).await?;
        if let Some(stored) = stored {
            schedules.push(parse_stored(stored)?);
        }
    }
    Ok(schedules)
}

fn missing_registered_schedule_ids(
    registry: &TaskBindingRegistry,
    present: &HashSet<String>,
) -> Result<Vec<String>, CliError> {
    validate_binding_registry(registry)?;
    registry
        .by_schedule
        .keys()
        .filter(|id| !present.contains(*id))
        .map(|id| validate_id(id))
        .collect()
}

fn only_due(
    schedules: Vec<LoadedSchedule>,
    now: DateTime<Utc>,
) -> Result<Vec<LoadedSchedule>, CliError> {
    schedules
        .into_iter()
        .filter_map(|loaded| match is_due(&loaded.schedule, now) {
            Ok(true) => Some(Ok(loaded)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

async fn load_one(
    client: &BuzzClient,
    owner: Option<&str>,
    id: &str,
) -> Result<(nostr::PublicKey, LoadedSchedule), CliError> {
    let id = validate_id(id)?;
    let slug = schedule_slug(&id);
    let (owner_pubkey, stored) = get_stored_memory(client, owner, &slug).await?;
    let stored = stored.ok_or_else(|| CliError::NotFound(format!("schedule not found: {id}")))?;
    Ok((owner_pubkey, parse_stored(stored)?))
}

fn is_due(schedule: &Schedule, now: DateTime<Utc>) -> Result<bool, CliError> {
    if schedule.status == ScheduleStatus::Completed {
        return Ok(false);
    }
    if parse_time(&schedule.due_at, "due-at")? > now {
        return Ok(false);
    }
    match (&schedule.status, &schedule.claim) {
        (ScheduleStatus::Pending, None) => Ok(true),
        (ScheduleStatus::Claimed, Some(claim)) => {
            Ok(parse_time(&claim.lease_expires_at, "lease-expires-at")? <= now)
        }
        _ => Err(CliError::Other(format!(
            "schedule `{}` has an inconsistent status/claim pair",
            schedule.id
        ))),
    }
}

fn claim_schedule(
    schedule: &mut Schedule,
    now: DateTime<Utc>,
    lease_seconds: i64,
) -> Result<Option<String>, CliError> {
    if !is_due(schedule, now)? {
        return Ok(None);
    }
    let token = Uuid::new_v4().simple().to_string();
    schedule.status = ScheduleStatus::Claimed;
    schedule.updated_at = canonical_time(now);
    schedule.claim = Some(Claim {
        token: token.clone(),
        claimed_at: canonical_time(now),
        lease_expires_at: canonical_time(now + chrono::Duration::seconds(lease_seconds)),
    });
    Ok(Some(token))
}

fn record_claim_write(
    claimed: &mut Vec<(Schedule, String)>,
    schedule: Schedule,
    result: Result<String, CliError>,
) -> Result<(), CliError> {
    match result {
        Ok(revision) => claimed.push((schedule, revision)),
        Err(CliError::Conflict(_)) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn require_live_claim<'a>(
    schedule: &'a Schedule,
    token: &str,
    now: DateTime<Utc>,
) -> Result<&'a Claim, CliError> {
    match (&schedule.status, &schedule.claim) {
        (ScheduleStatus::Claimed, Some(claim)) if claim.token == token => {
            if parse_time(&claim.lease_expires_at, "lease-expires-at")? <= now {
                return Err(CliError::Conflict(format!(
                    "schedule `{}` claim lease expired before the decision",
                    schedule.id
                )));
            }
            Ok(claim)
        }
        (ScheduleStatus::Claimed, Some(_)) => Err(CliError::Conflict(format!(
            "schedule `{}` is held by a different claim",
            schedule.id
        ))),
        _ => Err(CliError::Conflict(format!(
            "schedule `{}` is not currently claimed",
            schedule.id
        ))),
    }
}

fn print_one(schedule: &Schedule, revision: &str, idempotent: bool) -> Result<(), CliError> {
    println!(
        "{}",
        serde_json::to_string(&ScheduleOutput {
            schedule,
            revision,
            idempotent,
        })
        .map_err(|error| CliError::Other(format!("schedule output failed: {error}")))?
    );
    Ok(())
}

struct CreateInput<'a> {
    id: &'a str,
    due_at: &'a str,
    channel: &'a str,
    thread: &'a str,
    assignee: &'a str,
    delegation_event: &'a str,
    expected_result: &'a str,
    evidence_locator: &'a str,
    receipt: &'a str,
    material_at: &'a str,
    expected_cause: &'a str,
    action: &'a str,
    check: &'a str,
}

fn create_definition_matches(existing: &Schedule, requested: &Schedule) -> bool {
    existing.id == requested.id
        && existing.due_at == requested.due_at
        && existing.channel_id == requested.channel_id
        && existing.thread_id == requested.thread_id
        && existing.task == requested.task
        && existing.checkpoint == requested.checkpoint
        && existing.phase == requested.phase
        && existing.audit.is_empty()
        && existing.audit_archive.is_none()
        && existing.expected_cause == requested.expected_cause
        && existing.action == requested.action
        && existing.check == requested.check
        && existing.status == ScheduleStatus::Pending
        && existing.claim.is_none()
        && existing.last_transition.is_none()
}

async fn create(
    client: &BuzzClient,
    input: CreateInput<'_>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let id = validate_id(input.id)?;
    let decision_at = Utc::now();
    let parsed_due_at = parse_time(input.due_at, "due-at")?;
    let due_at = canonical_time(parsed_due_at);
    let channel_id = Uuid::parse_str(input.channel)
        .map_err(|_| CliError::Usage("--channel must be a UUID".into()))?
        .to_string();
    let thread_id = validate_thread_id(input.thread)?;
    let task = TaskBinding {
        assignee_pubkey: validate_pubkey(input.assignee, "assignee")?,
        delegation_event_id: validate_thread_id(input.delegation_event)?,
        expected_result: validate_task_text(input.expected_result, "expected-result")?,
        evidence_locator: validate_task_text(input.evidence_locator, "evidence-locator")?,
    };
    let material_at = parse_time(input.material_at, "material-at")?;
    if material_at > decision_at {
        return Err(CliError::Usage(
            "--material-at cannot be later than the schedule creation time".into(),
        ));
    }
    let checkpoint = MaterialCheckpoint {
        receipt: validate_receipt(input.receipt)?,
        material_at: canonical_time(material_at),
    };
    let expected_cause = validate_text(input.expected_cause, "expected-cause")?;
    let action = validate_text(input.action, "action")?;
    let check = validate_text(input.check, "check")?;
    let now = canonical_time(decision_at);
    let schedule = Schedule {
        schema: TASK_SCHEMA_VERSION,
        id: id.clone(),
        due_at: due_at.clone(),
        channel_id: channel_id.clone(),
        thread_id: thread_id.clone(),
        task: Some(task.clone()),
        checkpoint: Some(checkpoint.clone()),
        phase: Some(FollowThroughPhase::Monitoring),
        audit: Vec::new(),
        audit_archive: None,
        pending_action: None,
        expected_cause,
        action,
        check,
        status: ScheduleStatus::Pending,
        created_at: now.clone(),
        updated_at: now,
        claim: None,
        last_transition: None,
    };
    let slug = schedule_slug(&id);
    let (owner_pubkey, existing) = get_stored_memory(client, owner, &slug).await?;
    if let Some(existing) = existing {
        let loaded = parse_stored(existing)?;
        if create_definition_matches(&loaded.schedule, &schedule) {
            return print_one(&loaded.schedule, &loaded.revision, true);
        }
        return Err(CliError::Conflict(format!(
            "schedule `{id}` already exists with different state or instructions"
        )));
    }
    validate_next_due_at(decision_at, &due_at)?;
    fetch_and_validate_delegation(
        client,
        &task.delegation_event_id,
        &channel_id,
        &thread_id,
        &task,
    )
    .await?;
    verify_task_receipt(
        client,
        &channel_id,
        &thread_id,
        &task.assignee_pubkey,
        &checkpoint,
    )
    .await?;
    reserve_task_binding(client, &owner_pubkey, &task.delegation_event_id, &id).await?;
    let value = serialized_schedule(&schedule)?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &slug,
        value,
        ExpectedMemoryHead::Missing,
    )
    .await;
    match revision {
        Ok(revision) => print_one(&schedule, &revision, false),
        Err(CliError::Conflict(_)) => {
            let (_, stored) = get_stored_memory(client, owner, &slug).await?;
            let stored = stored.ok_or_else(|| {
                CliError::Conflict(format!(
                    "schedule `{id}` creation lost its head CAS without a readable winner"
                ))
            })?;
            let loaded = parse_stored(stored)?;
            if create_definition_matches(&loaded.schedule, &schedule) {
                print_one(&loaded.schedule, &loaded.revision, true)
            } else {
                Err(CliError::Conflict(format!(
                    "schedule `{id}` raced with a different definition"
                )))
            }
        }
        Err(error) => Err(error),
    }
}

async fn list(
    client: &BuzzClient,
    status: Option<ScheduleStatusArg>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let mut schedules = load_all(client, owner).await?;
    if let Some(status) = status {
        let expected = match status {
            ScheduleStatusArg::Pending => ScheduleStatus::Pending,
            ScheduleStatusArg::Claimed => ScheduleStatus::Claimed,
            ScheduleStatusArg::Completed => ScheduleStatus::Completed,
        };
        schedules.retain(|loaded| loaded.schedule.status == expected);
    }
    schedules.sort_by(|a, b| {
        a.schedule
            .due_at
            .cmp(&b.schedule.due_at)
            .then_with(|| a.schedule.id.cmp(&b.schedule.id))
    });
    let output: Vec<_> = schedules
        .iter()
        .map(|loaded| ScheduleOutput {
            schedule: &loaded.schedule,
            revision: &loaded.revision,
            idempotent: false,
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| CliError::Other(format!("schedule output failed: {error}")))?
    );
    Ok(())
}

async fn due(client: &BuzzClient, at: Option<&str>, owner: Option<&str>) -> Result<(), CliError> {
    let now = match at {
        Some(value) => parse_time(value, "at")?,
        None => Utc::now(),
    };
    let mut schedules = only_due(load_all(client, owner).await?, now)?;
    schedules.sort_by(|a, b| {
        a.schedule
            .due_at
            .cmp(&b.schedule.due_at)
            .then_with(|| a.schedule.id.cmp(&b.schedule.id))
    });
    let output: Vec<_> = schedules
        .iter()
        .map(|loaded| ScheduleOutput {
            schedule: &loaded.schedule,
            revision: &loaded.revision,
            idempotent: false,
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| CliError::Other(format!("schedule output failed: {error}")))?
    );
    Ok(())
}

async fn claim_due(
    client: &BuzzClient,
    at: Option<&str>,
    lease_seconds: u64,
    limit: usize,
    owner: Option<&str>,
) -> Result<(), CliError> {
    if lease_seconds == 0 || lease_seconds > 86_400 {
        return Err(CliError::Usage(
            "--lease-seconds must be between 1 and 86400".into(),
        ));
    }
    if limit == 0 || limit > 100 {
        return Err(CliError::Usage("--limit must be between 1 and 100".into()));
    }
    let now = match at {
        Some(value) => parse_time(value, "at")?,
        None => Utc::now(),
    };
    let mut candidates = only_due(load_all(client, owner).await?, now)?;
    candidates.sort_by(|a, b| {
        a.schedule
            .due_at
            .cmp(&b.schedule.due_at)
            .then_with(|| a.schedule.id.cmp(&b.schedule.id))
    });
    candidates.truncate(limit);

    let mut claimed: Vec<(Schedule, String)> = Vec::new();
    for candidate in candidates {
        let (owner_pubkey, mut current) = load_one(client, owner, &candidate.schedule.id).await?;
        if claim_schedule(&mut current.schedule, now, lease_seconds as i64)?.is_none() {
            continue;
        }
        let value = serialized_schedule(&current.schedule)?;
        let revision = put_stored_memory(
            client,
            &owner_pubkey,
            &current.slug,
            value,
            ExpectedMemoryHead::Event(&current.revision),
        )
        .await;
        record_claim_write(&mut claimed, current.schedule, revision)?;
    }
    let output: Vec<_> = claimed
        .iter()
        .map(|(schedule, revision)| ScheduleOutput {
            schedule,
            revision,
            idempotent: false,
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&output)
            .map_err(|error| CliError::Other(format!("schedule output failed: {error}")))?
    );
    Ok(())
}

fn task_bound_transition_error(transition: &str) -> CliError {
    CliError::Usage(format!(
        "task-bound schedules must use `buzz schedules reconcile` for {transition}"
    ))
}

fn legacy_completion_matches(schedule: &Schedule, token: &str) -> bool {
    schedule.schema == LEGACY_SCHEMA_VERSION
        && schedule.status == ScheduleStatus::Completed
        && schedule.claim.is_none()
        && schedule.last_transition.as_ref().is_some_and(|transition| {
            transition.kind == TransitionKind::Completed && transition.claim_token == token
        })
}

fn complete_legacy_schedule(
    schedule: &mut Schedule,
    token: &str,
    now: DateTime<Utc>,
) -> Result<bool, CliError> {
    if schedule.schema != LEGACY_SCHEMA_VERSION {
        return Err(task_bound_transition_error("completion"));
    }
    if legacy_completion_matches(schedule, token) {
        return Ok(true);
    }
    require_live_claim(schedule, token, now)?;
    schedule.status = ScheduleStatus::Completed;
    schedule.claim = None;
    schedule.updated_at = canonical_time(now);
    schedule.last_transition = Some(LastTransition {
        kind: TransitionKind::Completed,
        claim_token: token.to_owned(),
        at: canonical_time(now),
    });
    validate_schedule(schedule)?;
    Ok(false)
}

struct LegacyRescheduleInput<'a> {
    due_at: &'a str,
    expected_cause: Option<&'a str>,
    action: Option<&'a str>,
    check: Option<&'a str>,
}

fn legacy_reschedule_matches(
    schedule: &Schedule,
    token: &str,
    input: &LegacyRescheduleInput<'_>,
) -> bool {
    schedule.schema == LEGACY_SCHEMA_VERSION
        && schedule.status == ScheduleStatus::Pending
        && schedule.last_transition.as_ref().is_some_and(|transition| {
            transition.kind == TransitionKind::Rescheduled && transition.claim_token == token
        })
        && schedule.due_at == input.due_at
        && input
            .expected_cause
            .is_none_or(|value| schedule.expected_cause == value)
        && input.action.is_none_or(|value| schedule.action == value)
        && input.check.is_none_or(|value| schedule.check == value)
}

fn legacy_reschedule_candidate_matches(
    winner: &Schedule,
    candidate: &Schedule,
    token: &str,
) -> bool {
    legacy_reschedule_matches(
        winner,
        token,
        &LegacyRescheduleInput {
            due_at: &candidate.due_at,
            expected_cause: Some(&candidate.expected_cause),
            action: Some(&candidate.action),
            check: Some(&candidate.check),
        },
    )
}

fn reschedule_legacy_schedule(
    schedule: &mut Schedule,
    token: &str,
    now: DateTime<Utc>,
    input: LegacyRescheduleInput<'_>,
) -> Result<bool, CliError> {
    if schedule.schema != LEGACY_SCHEMA_VERSION {
        return Err(task_bound_transition_error("rescheduling"));
    }
    if legacy_reschedule_matches(schedule, token, &input) {
        return Ok(true);
    }
    require_live_claim(schedule, token, now)?;
    validate_next_due_at(now, input.due_at)?;
    schedule.due_at = input.due_at.to_owned();
    if let Some(value) = input.expected_cause {
        schedule.expected_cause = value.to_owned();
    }
    if let Some(value) = input.action {
        schedule.action = value.to_owned();
    }
    if let Some(value) = input.check {
        schedule.check = value.to_owned();
    }
    schedule.status = ScheduleStatus::Pending;
    schedule.claim = None;
    schedule.updated_at = canonical_time(now);
    schedule.last_transition = Some(LastTransition {
        kind: TransitionKind::Rescheduled,
        claim_token: token.to_owned(),
        at: canonical_time(now),
    });
    validate_schedule(schedule)?;
    Ok(false)
}

async fn complete(
    client: &BuzzClient,
    id: &str,
    claim: &str,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let (owner_pubkey, mut loaded) = load_one(client, owner, id).await?;
    let idempotent = complete_legacy_schedule(&mut loaded.schedule, claim, Utc::now())?;
    if idempotent {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    let value = serialized_schedule(&loaded.schedule)?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &loaded.slug,
        value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await;
    match revision {
        Ok(revision) => print_one(&loaded.schedule, &revision, false),
        Err(CliError::Conflict(_)) => {
            let (_, winner) = load_one(client, owner, id).await?;
            if legacy_completion_matches(&winner.schedule, claim) {
                print_one(&winner.schedule, &winner.revision, true)
            } else {
                Err(CliError::Conflict(format!(
                    "schedule `{id}` completion lost its head CAS to different state"
                )))
            }
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
async fn reschedule(
    client: &BuzzClient,
    id: &str,
    claim: &str,
    due_at: &str,
    expected_cause: Option<&str>,
    action: Option<&str>,
    check: Option<&str>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let due_at = canonical_time(parse_time(due_at, "due-at")?);
    let expected_cause = expected_cause
        .map(|value| validate_text(value, "expected-cause"))
        .transpose()?;
    let action = action
        .map(|value| validate_text(value, "action"))
        .transpose()?;
    let check = check
        .map(|value| validate_text(value, "check"))
        .transpose()?;
    let (owner_pubkey, mut loaded) = load_one(client, owner, id).await?;
    let input = LegacyRescheduleInput {
        due_at: &due_at,
        expected_cause: expected_cause.as_deref(),
        action: action.as_deref(),
        check: check.as_deref(),
    };
    let idempotent = reschedule_legacy_schedule(&mut loaded.schedule, claim, Utc::now(), input)?;
    if idempotent {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    let value = serialized_schedule(&loaded.schedule)?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &loaded.slug,
        value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await;
    match revision {
        Ok(revision) => print_one(&loaded.schedule, &revision, false),
        Err(CliError::Conflict(_)) => {
            let (_, winner) = load_one(client, owner, id).await?;
            if legacy_reschedule_candidate_matches(&winner.schedule, &loaded.schedule, claim) {
                print_one(&winner.schedule, &winner.revision, true)
            } else {
                Err(CliError::Conflict(format!(
                    "schedule `{id}` reschedule lost its head CAS to different state"
                )))
            }
        }
        Err(error) => Err(error),
    }
}

struct BindInput<'a> {
    id: &'a str,
    claim: &'a str,
    due_at: &'a str,
    assignee: &'a str,
    delegation_event: &'a str,
    expected_result: &'a str,
    evidence_locator: &'a str,
    receipt: &'a str,
    material_at: &'a str,
}

fn bind_schedule(
    schedule: &Schedule,
    claim: &str,
    now: DateTime<Utc>,
    due_at: String,
    task: TaskBinding,
    checkpoint: MaterialCheckpoint,
) -> Result<(Schedule, bool), CliError> {
    if bind_retry_matches(schedule, claim, &due_at, &task, &checkpoint) {
        return Ok((schedule.clone(), true));
    }
    if schedule.schema == TASK_SCHEMA_VERSION {
        return Err(CliError::Conflict(format!(
            "schedule `{}` is already task-bound with different state",
            schedule.id
        )));
    }
    require_live_claim(schedule, claim, now)?;
    validate_next_due_at(now, &due_at)?;
    let mut candidate = schedule.clone();
    candidate.schema = TASK_SCHEMA_VERSION;
    candidate.task = Some(task);
    candidate.checkpoint = Some(checkpoint);
    candidate.phase = Some(FollowThroughPhase::Monitoring);
    candidate.audit.clear();
    candidate.audit_archive = None;
    candidate.due_at = due_at;
    candidate.status = ScheduleStatus::Pending;
    candidate.claim = None;
    candidate.updated_at = canonical_time(now);
    candidate.last_transition = Some(LastTransition {
        kind: TransitionKind::Bound,
        claim_token: claim.to_owned(),
        at: canonical_time(now),
    });
    validate_schedule(&candidate)?;
    Ok((candidate, false))
}

fn bind_retry_matches(
    schedule: &Schedule,
    claim: &str,
    due_at: &str,
    task: &TaskBinding,
    checkpoint: &MaterialCheckpoint,
) -> bool {
    schedule.schema == TASK_SCHEMA_VERSION
        && schedule.status == ScheduleStatus::Pending
        && schedule.claim.is_none()
        && schedule.task.as_ref() == Some(task)
        && schedule.checkpoint.as_ref() == Some(checkpoint)
        && schedule.phase == Some(FollowThroughPhase::Monitoring)
        && schedule.due_at == due_at
        && schedule.audit.is_empty()
        && schedule.last_transition.as_ref().is_some_and(|transition| {
            transition.kind == TransitionKind::Bound && transition.claim_token == claim
        })
}

async fn bind(
    client: &BuzzClient,
    input: BindInput<'_>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let now = Utc::now();
    let due_at = canonical_time(parse_time(input.due_at, "due-at")?);
    let material_at = parse_time(input.material_at, "material-at")?;
    if material_at > now {
        return Err(CliError::Usage(
            "--material-at cannot be later than the bind decision".into(),
        ));
    }
    let task = TaskBinding {
        assignee_pubkey: validate_pubkey(input.assignee, "assignee")?,
        delegation_event_id: validate_thread_id(input.delegation_event)?,
        expected_result: validate_task_text(input.expected_result, "expected-result")?,
        evidence_locator: validate_task_text(input.evidence_locator, "evidence-locator")?,
    };
    let checkpoint = MaterialCheckpoint {
        receipt: validate_receipt(input.receipt)?,
        material_at: canonical_time(material_at),
    };
    let (owner_pubkey, loaded) = load_one(client, owner, input.id).await?;
    if bind_retry_matches(&loaded.schedule, input.claim, &due_at, &task, &checkpoint) {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    validate_next_due_at(now, &due_at)?;
    fetch_and_validate_delegation(
        client,
        &task.delegation_event_id,
        &loaded.schedule.channel_id,
        &loaded.schedule.thread_id,
        &task,
    )
    .await?;
    verify_task_receipt(
        client,
        &loaded.schedule.channel_id,
        &loaded.schedule.thread_id,
        &task.assignee_pubkey,
        &checkpoint,
    )
    .await?;
    reserve_task_binding(
        client,
        &owner_pubkey,
        &task.delegation_event_id,
        &loaded.schedule.id,
    )
    .await?;

    let (owner_pubkey, mut loaded) = load_one(client, owner, input.id).await?;
    let decision_at = Utc::now();
    let (schedule, idempotent) = bind_schedule(
        &loaded.schedule,
        input.claim,
        decision_at,
        due_at,
        task,
        checkpoint,
    )?;
    if idempotent {
        return print_one(&schedule, &loaded.revision, true);
    }
    loaded.schedule = schedule;
    let value = serialized_schedule(&loaded.schedule)?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &loaded.slug,
        value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await;
    match revision {
        Ok(revision) => print_one(&loaded.schedule, &revision, false),
        Err(CliError::Conflict(_)) => {
            let (_, winner) = load_one(client, owner, input.id).await?;
            if bind_retry_matches(
                &winner.schedule,
                input.claim,
                &loaded.schedule.due_at,
                loaded.schedule.task.as_ref().ok_or_else(|| {
                    CliError::Other("bound schedule lost its task binding".into())
                })?,
                loaded
                    .schedule
                    .checkpoint
                    .as_ref()
                    .ok_or_else(|| CliError::Other("bound schedule lost its checkpoint".into()))?,
            ) {
                print_one(&winner.schedule, &winner.revision, true)
            } else {
                Err(CliError::Conflict(format!(
                    "schedule `{}` bind lost its head CAS to different state",
                    loaded.schedule.id
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn archive_value(archive: &AuditArchive) -> Result<String, CliError> {
    let value = serde_json::to_string(archive)
        .map_err(|error| CliError::Other(format!("audit archive serialization failed: {error}")))?;
    let digest = sha256_hex(value.as_bytes());
    let slug = archive_slug(&archive.schedule_id, archive.sequence, &digest);
    let plaintext_len = engram_plaintext_len(&slug, value.clone());
    if plaintext_len > engram::NIP44_PLAINTEXT_MAX {
        return Err(CliError::Conflict(format!(
            "schedule `{}` audit archive requires {plaintext_len} NIP-AE plaintext bytes, above the {}-byte limit",
            archive.schedule_id,
            engram::NIP44_PLAINTEXT_MAX,
        )));
    }
    Ok(value)
}

fn plan_audit_rollover(schedule: &Schedule) -> Result<Option<(AuditArchive, usize)>, CliError> {
    let current = serialized_schedule(schedule)?;
    if engram_plaintext_len(&schedule_slug(&schedule.id), current) <= ACTIVE_HEAD_ROLLOVER_BYTES {
        return Ok(None);
    }
    if schedule.audit.len() < 2 {
        return Err(CliError::Conflict(format!(
            "schedule `{}` cannot roll over its audit while retaining the latest idempotency record",
            schedule.id
        )));
    }
    let sequence = schedule
        .audit_archive
        .as_ref()
        .map_or(1, |reference| reference.sequence.saturating_add(1));
    if sequence == u32::MAX {
        return Err(CliError::Conflict(format!(
            "schedule `{}` exhausted its bounded audit archive sequence",
            schedule.id
        )));
    }
    for drain_count in 1..schedule.audit.len() {
        let archive = AuditArchive {
            schema: 1,
            schedule_id: schedule.id.clone(),
            sequence,
            previous: schedule.audit_archive.clone(),
            entries: schedule.audit[..drain_count].to_vec(),
        };
        let archive_json = archive_value(&archive)?;
        let archive_digest = sha256_hex(archive_json.as_bytes());
        let mut candidate = schedule.clone();
        candidate.audit.drain(..drain_count);
        candidate.audit_archive = Some(AuditArchiveRef {
            sequence,
            slug: archive_slug(&schedule.id, sequence, &archive_digest),
            revision: "0".repeat(64),
            entry_count: archive.entries.len(),
        });
        let head = serialized_schedule(&candidate)?;
        if engram_plaintext_len(&schedule_slug(&schedule.id), head) <= ACTIVE_HEAD_ROLLOVER_BYTES {
            return Ok(Some((archive, drain_count)));
        }
    }
    Err(CliError::Conflict(format!(
        "schedule `{}` audit cannot be rolled over within the NIP-AE head bound",
        schedule.id
    )))
}

async fn persist_archive(
    client: &BuzzClient,
    owner: &nostr::PublicKey,
    archive: &AuditArchive,
) -> Result<String, CliError> {
    let value = archive_value(archive)?;
    let digest = sha256_hex(value.as_bytes());
    let slug = archive_slug(&archive.schedule_id, archive.sequence, &digest);
    let owner_hex = owner.to_hex();
    let (_, existing) = get_stored_memory(client, Some(&owner_hex), &slug).await?;
    if let Some(existing) = existing {
        if existing.value == value {
            return Ok(existing.event.id.to_hex());
        }
        return Err(CliError::Conflict(format!(
            "audit archive `{slug}` already exists with different immutable history"
        )));
    }
    match put_stored_memory(
        client,
        owner,
        &slug,
        value.clone(),
        ExpectedMemoryHead::Missing,
    )
    .await
    {
        Ok(revision) => Ok(revision),
        Err(CliError::Conflict(_)) => {
            let (_, raced) = get_stored_memory(client, Some(&owner_hex), &slug).await?;
            match raced {
                Some(existing) if existing.value == value => Ok(existing.event.id.to_hex()),
                _ => Err(CliError::Conflict(format!(
                    "audit archive `{slug}` raced with different immutable history"
                ))),
            }
        }
        Err(error) => Err(error),
    }
}

async fn roll_over_audit(
    client: &BuzzClient,
    owner: &nostr::PublicKey,
    schedule: &mut Schedule,
) -> Result<(), CliError> {
    while let Some((archive, drain_count)) = plan_audit_rollover(schedule)? {
        let revision = persist_archive(client, owner, &archive).await?;
        schedule.audit.drain(..drain_count);
        schedule.audit_archive = Some(AuditArchiveRef {
            sequence: archive.sequence,
            slug: archive_slug(
                &schedule.id,
                archive.sequence,
                &sha256_hex(archive_value(&archive)?.as_bytes()),
            ),
            revision,
            entry_count: archive.entries.len(),
        });
        validate_schedule(schedule)?;
    }
    Ok(())
}

#[derive(Clone)]
struct ReconcileInput {
    decision: ScheduleDecision,
    receipt: String,
    material_at: String,
    due_at: Option<String>,
    replacement_pubkey: Option<String>,
    replacement_delegation_event_id: Option<String>,
    action_event_id: Option<String>,
    action_content: Option<String>,
}

fn require_due_at(input: &ReconcileInput) -> Result<&str, CliError> {
    input.due_at.as_deref().ok_or_else(|| {
        CliError::Usage("--due-at is required for keep, wake, and redirect decisions".into())
    })
}

fn reject_replacement(input: &ReconcileInput) -> Result<(), CliError> {
    if input.replacement_pubkey.is_some() || input.replacement_delegation_event_id.is_some() {
        return Err(CliError::Usage(
            "replacement metadata is valid only for redirect".into(),
        ));
    }
    Ok(())
}

fn require_visible_action(input: &ReconcileInput) -> Result<(&str, &str), CliError> {
    let event_id = input.action_event_id.as_deref().ok_or_else(|| {
        CliError::Usage("wake, redirect, and complete require a prepared action event".into())
    })?;
    let content = input
        .action_content
        .as_deref()
        .ok_or_else(|| CliError::Usage("wake, redirect, and complete require --message".into()))?;
    validate_thread_id(event_id)?;
    validate_text(content, "message")?;
    Ok((event_id, content))
}

fn validate_public_reconcile_shape(input: &ReconcileInput) -> Result<(), CliError> {
    match input.decision {
        ScheduleDecision::Keep => {
            require_due_at(input)?;
            reject_replacement(input)?;
            if input.action_content.is_some() {
                return Err(CliError::Usage("keep must not include --message".into()));
            }
        }
        ScheduleDecision::Wake => {
            require_due_at(input)?;
            reject_replacement(input)?;
            if input.action_content.is_none() {
                return Err(CliError::Usage("wake requires --message".into()));
            }
        }
        ScheduleDecision::Redirect => {
            require_due_at(input)?;
            if input.replacement_pubkey.is_none() {
                return Err(CliError::Usage("redirect requires --replacement".into()));
            }
            if input.action_content.is_none() {
                return Err(CliError::Usage("redirect requires --message".into()));
            }
        }
        ScheduleDecision::Completed => {
            reject_replacement(input)?;
            if input.due_at.is_some() {
                return Err(CliError::Usage(
                    "--due-at must be omitted for a complete decision".into(),
                ));
            }
            if input.action_content.is_none() {
                return Err(CliError::Usage("complete requires --message".into()));
            }
        }
    }
    Ok(())
}

fn checkpoint_matches(schedule: &Schedule, input: &ReconcileInput) -> bool {
    schedule.checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.receipt == input.receipt && checkpoint.material_at == input.material_at
    })
}

fn validate_action_event_scope(
    event: &nostr::Event,
    driver: &nostr::PublicKey,
    channel_id: &str,
    thread_id: &str,
    expected_mentions: &[&str],
) -> Result<(), CliError> {
    let kind = event.kind.as_u16() as u32;
    if kind != KIND_STREAM_MESSAGE && kind != KIND_STREAM_MESSAGE_V2 {
        return Err(CliError::Usage(
            "follow-through action must be a Buzz stream message".into(),
        ));
    }
    if &event.pubkey != driver {
        return Err(CliError::Usage(
            "follow-through action must be authored by the schedule driver".into(),
        ));
    }
    if event.content.trim().is_empty() {
        return Err(CliError::Usage(
            "follow-through action message cannot be empty".into(),
        ));
    }
    let channels: Vec<&str> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
        .filter_map(|tag| tag.content())
        .collect();
    if channels.as_slice() != [channel_id] {
        return Err(CliError::Usage(
            "follow-through action must carry exactly the task channel h-tag".into(),
        ));
    }
    let root = buzz_core::nip10::parse_thread_markers(&event.tags)
        .resolve()
        .map(|(root, _)| root);
    if root.as_deref() != Some(thread_id) {
        return Err(CliError::Usage(
            "follow-through action must reply directly in the exact task thread".into(),
        ));
    }
    let mut mentions: Vec<String> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("p"))
        .filter_map(|tag| tag.content())
        .map(str::to_ascii_lowercase)
        .collect();
    mentions.sort();
    mentions.dedup();
    let mut expected: Vec<String> = expected_mentions
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    expected.sort();
    expected.dedup();
    if mentions != expected {
        return Err(CliError::Usage(
            "follow-through action mentions do not match the task decision".into(),
        ));
    }
    Ok(())
}

fn task_state_tags(task: &TaskBinding, decision: ScheduleDecision) -> Vec<Vec<String>> {
    vec![
        vec![
            "d".to_owned(),
            format!("{TASK_STATE_PREFIX}{}", task.delegation_event_id),
        ],
        vec!["task-status".to_owned(), decision.task_state().to_owned()],
    ]
}

fn build_action_event(
    client: &BuzzClient,
    schedule: &Schedule,
    input: &ReconcileInput,
    now: DateTime<Utc>,
) -> Result<nostr::Event, CliError> {
    let content = input
        .action_content
        .as_deref()
        .ok_or_else(|| CliError::Usage("wake, redirect, and complete require --message".into()))?;
    validate_text(content, "message")?;
    let channel = Uuid::parse_str(&schedule.channel_id)
        .map_err(|_| CliError::Other("stored schedule channel is invalid".into()))?;
    let root = nostr::EventId::from_hex(&schedule.thread_id)
        .map_err(|_| CliError::Other("stored schedule thread is invalid".into()))?;
    let thread = buzz_sdk::ThreadRef {
        root_event_id: root,
        parent_event_id: root,
    };
    let task = schedule
        .task
        .as_ref()
        .ok_or_else(|| CliError::Usage("legacy schedules must be bound first".into()))?;
    let mentions: Vec<&str> = match input.decision {
        ScheduleDecision::Wake => vec![task.assignee_pubkey.as_str()],
        ScheduleDecision::Redirect => vec![input
            .replacement_pubkey
            .as_deref()
            .ok_or_else(|| CliError::Usage("--replacement is required for redirect".into()))?],
        ScheduleDecision::Completed => Vec::new(),
        ScheduleDecision::Keep => {
            return Err(CliError::Usage(
                "keep decisions do not create visible action events".into(),
            ));
        }
    };
    let created_at = u64::try_from(now.timestamp())
        .map_err(|_| CliError::Other("decision time predates the Nostr epoch".into()))?;
    let task_state_tags = task_state_tags(task, input.decision);
    let builder = buzz_sdk::build_message(
        channel,
        content,
        Some(&thread),
        &mentions,
        false,
        &task_state_tags,
    )
    .map_err(|error| CliError::Other(format!("action message build failed: {error}")))?
    .custom_created_at(nostr::Timestamp::from(created_at));
    let event = client.sign_event(builder)?;
    match input.decision {
        ScheduleDecision::Redirect => {
            let replacement_task = TaskBinding {
                assignee_pubkey: input.replacement_pubkey.clone().ok_or_else(|| {
                    CliError::Usage("--replacement is required for redirect".into())
                })?,
                delegation_event_id: event.id.to_hex(),
                expected_result: task.expected_result.clone(),
                evidence_locator: task.evidence_locator.clone(),
            };
            validate_delegation_event(
                &event,
                &client.keys().public_key(),
                &schedule.channel_id,
                &schedule.thread_id,
                &replacement_task,
            )?;
        }
        ScheduleDecision::Wake => validate_action_event_scope(
            &event,
            &client.keys().public_key(),
            &schedule.channel_id,
            &schedule.thread_id,
            &[task.assignee_pubkey.as_str()],
        )?,
        ScheduleDecision::Completed => validate_action_event_scope(
            &event,
            &client.keys().public_key(),
            &schedule.channel_id,
            &schedule.thread_id,
            &[],
        )?,
        ScheduleDecision::Keep => {
            return Err(CliError::Usage(
                "keep decisions do not create visible action events".into(),
            ));
        }
    }
    Ok(event)
}

fn reconcile_schedule(
    schedule: &Schedule,
    token: &str,
    now: DateTime<Utc>,
    input: &ReconcileInput,
    enforce_claim_expiry: bool,
) -> Result<(Schedule, bool), CliError> {
    if let Some(last) = schedule
        .audit
        .last()
        .filter(|entry| entry.claim_token == token)
    {
        let same = last.decision == input.decision
            && last.receipt == input.receipt
            && last.material_at == input.material_at
            && last.next_due_at == input.due_at
            && last.replacement_pubkey == input.replacement_pubkey
            && last.replacement_delegation_event_id == input.replacement_delegation_event_id
            && last.action_event_id == input.action_event_id
            && last.action_content == input.action_content;
        if same {
            return Ok((schedule.clone(), true));
        }
        return Err(CliError::Conflict(format!(
            "schedule `{}` claim already recorded a different reconciliation decision",
            schedule.id
        )));
    }

    if enforce_claim_expiry {
        require_live_claim(schedule, token, now)?;
    } else {
        match (&schedule.status, &schedule.claim) {
            (ScheduleStatus::Claimed, Some(claim)) if claim.token == token => {}
            (ScheduleStatus::Claimed, Some(_)) => {
                return Err(CliError::Conflict(format!(
                    "schedule `{}` is held by a different claim",
                    schedule.id
                )));
            }
            _ => {
                return Err(CliError::Conflict(format!(
                    "schedule `{}` is not currently claimed",
                    schedule.id
                )));
            }
        }
    }
    validate_receipt(&input.receipt)?;
    let material_at = parse_time(&input.material_at, "material-at")?;
    if material_at > now {
        return Err(CliError::Usage(
            "--material-at cannot be later than the reconciliation decision".into(),
        ));
    }
    if let Some(due_at) = &input.due_at {
        validate_next_due_at(now, due_at)?;
    }
    let task = schedule.task.clone().ok_or_else(|| {
        CliError::Usage("legacy schedules must use complete/reschedule, not reconcile".into())
    })?;
    let phase = schedule.phase.ok_or_else(|| {
        CliError::Other(format!(
            "task-bound schedule `{}` has no follow-through phase",
            schedule.id
        ))
    })?;
    if phase == FollowThroughPhase::Completed || schedule.status == ScheduleStatus::Completed {
        return Err(CliError::Conflict(format!(
            "schedule `{}` is already complete",
            schedule.id
        )));
    }

    let mut candidate = schedule.clone();
    let decision_at = canonical_time(now);
    let checkpoint = MaterialCheckpoint {
        receipt: input.receipt.clone(),
        material_at: input.material_at.clone(),
    };

    match input.decision {
        ScheduleDecision::Keep => {
            reject_replacement(input)?;
            if input.action_event_id.is_some() || input.action_content.is_some() {
                return Err(CliError::Usage(
                    "keep must not include a visible action message".into(),
                ));
            }
            let due_at = require_due_at(input)?;
            if now.signed_duration_since(material_at)
                > chrono::Duration::seconds(MAX_KEEP_MATERIAL_AGE_SECONDS)
            {
                return Err(CliError::Conflict(
                    "keep requires task-bound material no more than 15 minutes old; stale evidence must not defer recovery"
                        .into(),
                ));
            }
            if let Some(previous) = &schedule.checkpoint {
                let previous_at = parse_time(&previous.material_at, "material-at")?;
                let next_at = parse_time(&input.material_at, "material-at")?;
                if previous.receipt == input.receipt || next_at <= previous_at {
                    return Err(CliError::Conflict(
                        "keep requires a different receipt with a later material timestamp; generic presence or unchanged work is not progress"
                            .into(),
                    ));
                }
            }
            candidate.checkpoint = Some(checkpoint);
            candidate.phase = Some(FollowThroughPhase::Monitoring);
            candidate.due_at = due_at.to_owned();
            candidate.status = ScheduleStatus::Pending;
        }
        ScheduleDecision::Wake => {
            reject_replacement(input)?;
            require_visible_action(input)?;
            let due_at = require_due_at(input)?;
            if phase != FollowThroughPhase::Monitoring {
                return Err(CliError::Conflict(
                    "the same assignee was already woken for this unchanged checkpoint; the next unchanged decision must redirect"
                        .into(),
                ));
            }
            if schedule.checkpoint.is_some() && !checkpoint_matches(schedule, input) {
                return Err(CliError::Conflict(
                    "a changed receipt cannot be classified as a wake; use keep for newer material progress"
                        .into(),
                ));
            }
            candidate.checkpoint = Some(checkpoint);
            candidate.phase = Some(FollowThroughPhase::SameOwnerWoken);
            candidate.due_at = due_at.to_owned();
            candidate.status = ScheduleStatus::Pending;
        }
        ScheduleDecision::Redirect => {
            let (action_event_id, _) = require_visible_action(input)?;
            let due_at = require_due_at(input)?;
            if phase != FollowThroughPhase::SameOwnerWoken || !checkpoint_matches(schedule, input) {
                return Err(CliError::Conflict(
                    "redirect requires the exact checkpoint to remain unchanged after one same-owner wake"
                        .into(),
                ));
            }
            let replacement = input
                .replacement_pubkey
                .as_ref()
                .ok_or_else(|| CliError::Usage("--replacement is required for redirect".into()))?;
            let replacement_event =
                input
                    .replacement_delegation_event_id
                    .as_ref()
                    .ok_or_else(|| {
                        CliError::Other(
                            "prepared redirect is missing its generated delegation event".into(),
                        )
                    })?;
            if replacement == &task.assignee_pubkey {
                return Err(CliError::Conflict(
                    "redirect requires exactly one different assignee".into(),
                ));
            }
            if replacement_event == &task.delegation_event_id {
                return Err(CliError::Conflict(
                    "redirect requires a new delegation event for the replacement".into(),
                ));
            }
            if replacement_event != action_event_id {
                return Err(CliError::Conflict(
                    "redirect delegation event must be the prepared visible action event".into(),
                ));
            }
            candidate.task = Some(TaskBinding {
                assignee_pubkey: replacement.clone(),
                delegation_event_id: replacement_event.clone(),
                expected_result: task.expected_result.clone(),
                evidence_locator: task.evidence_locator.clone(),
            });
            candidate.checkpoint = Some(checkpoint);
            candidate.phase = Some(FollowThroughPhase::Monitoring);
            candidate.due_at = due_at.to_owned();
            candidate.status = ScheduleStatus::Pending;
        }
        ScheduleDecision::Completed => {
            reject_replacement(input)?;
            require_visible_action(input)?;
            if input.due_at.is_some() {
                return Err(CliError::Usage(
                    "--due-at must be omitted for a complete decision".into(),
                ));
            }
            if let Some(previous) = &schedule.checkpoint {
                let previous_at = parse_time(&previous.material_at, "material-at")?;
                if previous.receipt == input.receipt || material_at <= previous_at {
                    return Err(CliError::Conflict(
                        "complete requires a different result receipt with a later material timestamp"
                            .into(),
                    ));
                }
            }
            candidate.checkpoint = Some(checkpoint);
            candidate.phase = Some(FollowThroughPhase::Completed);
            candidate.status = ScheduleStatus::Completed;
        }
    }

    candidate.audit.push(DecisionAudit {
        claim_token: token.to_owned(),
        decision: input.decision,
        at: decision_at.clone(),
        assignee_pubkey: task.assignee_pubkey,
        delegation_event_id: task.delegation_event_id,
        receipt: input.receipt.clone(),
        material_at: input.material_at.clone(),
        next_due_at: input.due_at.clone(),
        replacement_pubkey: input.replacement_pubkey.clone(),
        replacement_delegation_event_id: input.replacement_delegation_event_id.clone(),
        action_event_id: input.action_event_id.clone(),
        action_content: input.action_content.clone(),
    });
    candidate.pending_action = None;
    candidate.claim = None;
    candidate.updated_at = decision_at.clone();
    candidate.last_transition = Some(LastTransition {
        kind: if input.decision == ScheduleDecision::Completed {
            TransitionKind::Completed
        } else {
            TransitionKind::Rescheduled
        },
        claim_token: token.to_owned(),
        at: decision_at,
    });
    validate_schedule(&candidate)?;
    Ok((candidate, false))
}

fn pending_matches_request(pending: &PendingAction, input: &ReconcileInput) -> bool {
    pending.decision == input.decision
        && pending.receipt == input.receipt
        && pending.material_at == input.material_at
        && pending.next_due_at == input.due_at
        && pending.replacement_pubkey == input.replacement_pubkey
        && input.action_content.as_deref() == Some(pending.event.content.as_str())
}

fn input_for_pending(pending: &PendingAction) -> ReconcileInput {
    ReconcileInput {
        decision: pending.decision,
        receipt: pending.receipt.clone(),
        material_at: pending.material_at.clone(),
        due_at: pending.next_due_at.clone(),
        replacement_pubkey: pending.replacement_pubkey.clone(),
        replacement_delegation_event_id: (pending.decision == ScheduleDecision::Redirect)
            .then(|| pending.event.id.to_hex()),
        action_event_id: Some(pending.event.id.to_hex()),
        action_content: Some(pending.event.content.clone()),
    }
}

fn prepare_visible_action(
    schedule: &Schedule,
    token: &str,
    now: DateTime<Utc>,
    requested: &ReconcileInput,
    event: nostr::Event,
) -> Result<(Schedule, ReconcileInput), CliError> {
    if requested.decision == ScheduleDecision::Keep {
        return Err(CliError::Usage(
            "keep decisions do not use the visible-action outbox".into(),
        ));
    }
    let mut input = requested.clone();
    input.action_event_id = Some(event.id.to_hex());
    input.action_content = Some(event.content.clone());
    if input.decision == ScheduleDecision::Redirect {
        input.replacement_delegation_event_id = Some(event.id.to_hex());
    }
    reconcile_schedule(schedule, token, now, &input, true)?;
    let task = schedule
        .task
        .as_ref()
        .ok_or_else(|| CliError::Usage("legacy schedules must be bound first".into()))?;
    let mut prepared = schedule.clone();
    prepared.pending_action = Some(PendingAction {
        prepared_claim_token: token.to_owned(),
        decision: input.decision,
        prepared_at: canonical_time(now),
        receipt: input.receipt.clone(),
        material_at: input.material_at.clone(),
        next_due_at: input.due_at.clone(),
        assignee_pubkey: task.assignee_pubkey.clone(),
        delegation_event_id: task.delegation_event_id.clone(),
        replacement_pubkey: input.replacement_pubkey.clone(),
        event,
    });
    prepared.updated_at = canonical_time(now);
    validate_schedule(&prepared)?;
    Ok((prepared, input))
}

fn finalize_pending_action(schedule: &Schedule, token: &str) -> Result<(Schedule, bool), CliError> {
    let pending = schedule.pending_action.as_ref().ok_or_else(|| {
        CliError::Conflict(format!(
            "schedule `{}` has no prepared visible action",
            schedule.id
        ))
    })?;
    let prepared_at = parse_time(&pending.prepared_at, "prepared-at")?;
    let input = input_for_pending(pending);
    reconcile_schedule(schedule, token, prepared_at, &input, false)
}

async fn advance_pending_redirect_binding(
    client: &BuzzClient,
    owner: &nostr::PublicKey,
    schedule: &Schedule,
) -> Result<(), CliError> {
    let Some(pending) = schedule
        .pending_action
        .as_ref()
        .filter(|pending| pending.decision == ScheduleDecision::Redirect)
    else {
        return Ok(());
    };
    advance_task_binding(
        client,
        owner,
        &schedule.id,
        &pending.delegation_event_id,
        &pending.event.id.to_hex(),
    )
    .await
}

fn recorded_request_matches(
    schedule: &Schedule,
    token: &str,
    input: &ReconcileInput,
) -> Result<bool, CliError> {
    let Some(last) = schedule
        .audit
        .last()
        .filter(|entry| entry.claim_token == token)
    else {
        return Ok(false);
    };
    let same = last.decision == input.decision
        && last.receipt == input.receipt
        && last.material_at == input.material_at
        && last.next_due_at == input.due_at
        && last.replacement_pubkey == input.replacement_pubkey
        && last.action_content == input.action_content;
    if same {
        Ok(true)
    } else {
        Err(CliError::Conflict(format!(
            "schedule `{}` claim already recorded a different reconciliation decision",
            schedule.id
        )))
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile(
    client: &BuzzClient,
    id: &str,
    claim: &str,
    decision: ScheduleDecisionArg,
    receipt: &str,
    material_at: &str,
    due_at: Option<&str>,
    replacement: Option<&str>,
    message: Option<&str>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let requested_at = Utc::now();
    let material_at = parse_time(material_at, "material-at")?;
    let input = ReconcileInput {
        decision: decision.into(),
        receipt: validate_receipt(receipt)?,
        material_at: canonical_time(material_at),
        due_at: due_at
            .map(|value| parse_time(value, "due-at").map(canonical_time))
            .transpose()?,
        replacement_pubkey: replacement
            .map(|value| validate_pubkey(value, "replacement"))
            .transpose()?,
        replacement_delegation_event_id: None,
        action_event_id: None,
        action_content: message
            .map(|value| validate_text(value, "message"))
            .transpose()?,
    };
    validate_public_reconcile_shape(&input)?;
    let (owner_pubkey, mut loaded) = load_one(client, owner, id).await?;
    if recorded_request_matches(&loaded.schedule, claim, &input)? {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    if let Some(pending) = loaded.schedule.pending_action.as_ref() {
        if !pending_matches_request(pending, &input) {
            return Err(CliError::Conflict(format!(
                "schedule `{}` already has a different prepared visible action",
                loaded.schedule.id
            )));
        }
        match (&loaded.schedule.status, &loaded.schedule.claim) {
            (ScheduleStatus::Claimed, Some(current)) if current.token == claim => {}
            _ => {
                return Err(CliError::Conflict(format!(
                    "schedule `{}` pending action is held by another claim",
                    loaded.schedule.id
                )));
            }
        }
        advance_pending_redirect_binding(client, &owner_pubkey, &loaded.schedule).await?;
        client.submit_event(pending.event.clone()).await?;
        let (mut finalized, _) = finalize_pending_action(&loaded.schedule, claim)?;
        roll_over_audit(client, &owner_pubkey, &mut finalized).await?;
        let value = serialized_schedule(&finalized)?;
        let revision = put_stored_memory(
            client,
            &owner_pubkey,
            &loaded.slug,
            value,
            ExpectedMemoryHead::Event(&loaded.revision),
        )
        .await?;
        return print_one(&finalized, &revision, false);
    }

    let current_task = loaded.schedule.task.as_ref().ok_or_else(|| {
        CliError::Usage("legacy schedules must be bound before reconciliation".into())
    })?;
    if material_at > requested_at {
        return Err(CliError::Usage(
            "--material-at cannot be later than the reconciliation decision".into(),
        ));
    }
    verify_task_receipt(
        client,
        &loaded.schedule.channel_id,
        &loaded.schedule.thread_id,
        &current_task.assignee_pubkey,
        &MaterialCheckpoint {
            receipt: input.receipt.clone(),
            material_at: input.material_at.clone(),
        },
    )
    .await?;

    let (fresh_owner, fresh) = load_one(client, owner, id).await?;
    loaded = fresh;
    if recorded_request_matches(&loaded.schedule, claim, &input)? {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    if loaded.schedule.pending_action.is_some() {
        return Err(CliError::Conflict(format!(
            "schedule `{}` gained a pending action; retry from fresh state",
            loaded.schedule.id
        )));
    }
    let decision_at = Utc::now();
    if input.decision == ScheduleDecision::Keep {
        let (mut schedule, idempotent) =
            reconcile_schedule(&loaded.schedule, claim, decision_at, &input, true)?;
        if idempotent {
            return print_one(&schedule, &loaded.revision, true);
        }
        roll_over_audit(client, &fresh_owner, &mut schedule).await?;
        let value = serialized_schedule(&schedule)?;
        let revision = put_stored_memory(
            client,
            &fresh_owner,
            &loaded.slug,
            value,
            ExpectedMemoryHead::Event(&loaded.revision),
        )
        .await?;
        return print_one(&schedule, &revision, false);
    }

    let event = build_action_event(client, &loaded.schedule, &input, decision_at)?;
    let (prepared, _) =
        prepare_visible_action(&loaded.schedule, claim, decision_at, &input, event)?;
    let prepared_value = serialized_schedule(&prepared)?;
    let prepared_revision = put_stored_memory(
        client,
        &fresh_owner,
        &loaded.slug,
        prepared_value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await?;
    advance_pending_redirect_binding(client, &fresh_owner, &prepared).await?;
    let pending = prepared
        .pending_action
        .as_ref()
        .ok_or_else(|| CliError::Other("prepared action disappeared".into()))?;
    client.submit_event(pending.event.clone()).await?;
    let (mut finalized, _) = finalize_pending_action(&prepared, claim)?;
    roll_over_audit(client, &fresh_owner, &mut finalized).await?;
    let final_value = serialized_schedule(&finalized)?;
    let final_revision = put_stored_memory(
        client,
        &fresh_owner,
        &loaded.slug,
        final_value,
        ExpectedMemoryHead::Event(&prepared_revision),
    )
    .await?;
    print_one(&finalized, &final_revision, false)
}

pub async fn dispatch(command: SchedulesCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
        SchedulesCmd::Adopt {
            source_event,
            due_at,
            expected_result,
            evidence_locator,
            owner,
        } => {
            adopt(
                client,
                &source_event,
                &due_at,
                &expected_result,
                &evidence_locator,
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::Create {
            id,
            due_at,
            channel,
            thread,
            assignee,
            delegation_event,
            expected_result,
            evidence_locator,
            receipt,
            material_at,
            expected_cause,
            action,
            check,
            owner,
        } => {
            create(
                client,
                CreateInput {
                    id: &id,
                    due_at: &due_at,
                    channel: &channel,
                    thread: &thread,
                    assignee: &assignee,
                    delegation_event: &delegation_event,
                    expected_result: &expected_result,
                    evidence_locator: &evidence_locator,
                    receipt: &receipt,
                    material_at: &material_at,
                    expected_cause: &expected_cause,
                    action: &action,
                    check: &check,
                },
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::List { status, owner } => list(client, status, owner.as_deref()).await,
        SchedulesCmd::Assigned {
            include_closed,
            since,
            limit,
        } => assigned(client, include_closed, since, limit).await,
        SchedulesCmd::Due { at, owner } => due(client, at.as_deref(), owner.as_deref()).await,
        SchedulesCmd::ClaimDue {
            at,
            lease_seconds,
            limit,
            owner,
        } => {
            claim_due(
                client,
                at.as_deref(),
                lease_seconds,
                limit,
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::Bind {
            id,
            claim,
            due_at,
            assignee,
            delegation_event,
            expected_result,
            evidence_locator,
            receipt,
            material_at,
            owner,
        } => {
            bind(
                client,
                BindInput {
                    id: &id,
                    claim: &claim,
                    due_at: &due_at,
                    assignee: &assignee,
                    delegation_event: &delegation_event,
                    expected_result: &expected_result,
                    evidence_locator: &evidence_locator,
                    receipt: &receipt,
                    material_at: &material_at,
                },
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::Complete { id, claim, owner } => {
            complete(client, &id, &claim, owner.as_deref()).await
        }
        SchedulesCmd::Reschedule {
            id,
            claim,
            due_at,
            expected_cause,
            action,
            check,
            owner,
        } => {
            reschedule(
                client,
                &id,
                &claim,
                &due_at,
                expected_cause.as_deref(),
                action.as_deref(),
                check.as_deref(),
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::Reconcile {
            id,
            claim,
            decision,
            receipt,
            material_at,
            due_at,
            replacement,
            message,
            owner,
        } => {
            reconcile(
                client,
                &id,
                &claim,
                decision,
                &receipt,
                &material_at,
                due_at.as_deref(),
                replacement.as_deref(),
                message.as_deref(),
                owner.as_deref(),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::engram::{build_event, validate_and_decrypt, Body};
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn sample_legacy_schedule(now: DateTime<Utc>) -> Schedule {
        Schedule::from(LegacyScheduleV1 {
            schema: LEGACY_SCHEMA_VERSION,
            id: "legacy-id".into(),
            due_at: canonical_time(now - chrono::Duration::minutes(1)),
            channel_id: "94b69f8a-59ab-4bd7-a049-e898ae1f624e".into(),
            thread_id: "a".repeat(64),
            expected_cause: "the delegated result or the check is due".into(),
            action: "inspect exact evidence and recover when needed".into(),
            check: "read the named evidence".into(),
            status: ScheduleStatus::Pending,
            created_at: canonical_time(now - chrono::Duration::hours(1)),
            updated_at: canonical_time(now - chrono::Duration::hours(1)),
            claim: None,
            last_transition: None,
        })
    }

    fn sample_schedule(now: DateTime<Utc>) -> Schedule {
        let assignee = Keys::generate().public_key().to_hex();
        Schedule {
            schema: TASK_SCHEMA_VERSION,
            id: "same-id".into(),
            due_at: canonical_time(now - chrono::Duration::minutes(1)),
            channel_id: "94b69f8a-59ab-4bd7-a049-e898ae1f624e".into(),
            thread_id: "a".repeat(64),
            task: Some(TaskBinding {
                assignee_pubkey: assignee,
                delegation_event_id: "b".repeat(64),
                expected_result: "the requested bounded result".into(),
                evidence_locator: "/workspace/driver-worktree".into(),
            }),
            checkpoint: Some(MaterialCheckpoint {
                receipt: format!("document-hash:{}", "1".repeat(64)),
                material_at: canonical_time(now - chrono::Duration::minutes(5)),
            }),
            phase: Some(FollowThroughPhase::Monitoring),
            audit: Vec::new(),
            audit_archive: None,
            pending_action: None,
            expected_cause: "the named assignee returns the requested result or 15 minutes pass"
                .into(),
            action:
                "stay silent for a newer receipt; otherwise wake once, then redirect exactly once"
                    .into(),
            check: "read the thread and /workspace/driver-worktree before any recovery".into(),
            status: ScheduleStatus::Pending,
            created_at: canonical_time(now),
            updated_at: canonical_time(now),
            claim: None,
            last_transition: None,
        }
    }

    #[test]
    fn lifecycle_survives_serialization_and_transitions_are_idempotent() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        assert!(!is_due(&schedule, now + chrono::Duration::minutes(5)).unwrap());

        let serialized = serde_json::to_string(&schedule).unwrap();
        let after_restart: Schedule = serde_json::from_str(&serialized).unwrap();
        let complete = reconciliation(
            ScheduleDecision::Completed,
            &format!("document-hash:{}", "2".repeat(64)),
            now + chrono::Duration::minutes(6),
            None,
        );
        let decision_at = now + chrono::Duration::minutes(6);
        let (completed, idempotent) =
            reconcile_schedule(&after_restart, &claim, decision_at, &complete, true).unwrap();
        assert!(!idempotent);
        let (retried, idempotent) =
            reconcile_schedule(&completed, &claim, decision_at, &complete, true).unwrap();
        assert!(idempotent);
        assert_eq!(retried, completed);
        assert!(!is_due(&completed, now + chrono::Duration::days(1)).unwrap());
    }

    #[test]
    fn driver_neutral_item_preserves_task_binding_across_restart() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = sample_schedule(now);
        let serialized = serde_json::to_string(&schedule).unwrap();
        let after_restart: Schedule = serde_json::from_str(&serialized).unwrap();

        let task = after_restart.task.expect("task binding");
        assert_eq!(task.delegation_event_id, "b".repeat(64));
        assert!(task.expected_result.contains("bounded result"));
        assert_eq!(task.evidence_locator, "/workspace/driver-worktree");
        assert!(after_restart.expected_cause.contains("named assignee"));
        assert!(after_restart.check.contains("/workspace/driver-worktree"));
        assert!(after_restart.action.contains("stay silent"));
        assert!(after_restart.action.contains("wake once"));
        assert!(after_restart.action.contains("redirect exactly once"));
    }

    fn reconciliation(
        decision: ScheduleDecision,
        receipt: &str,
        material_at: DateTime<Utc>,
        due_at: Option<DateTime<Utc>>,
    ) -> ReconcileInput {
        let visible = decision != ScheduleDecision::Keep;
        ReconcileInput {
            decision,
            receipt: receipt.into(),
            material_at: canonical_time(material_at),
            due_at: due_at.map(canonical_time),
            replacement_pubkey: None,
            replacement_delegation_event_id: None,
            action_event_id: visible.then(|| "d".repeat(64)),
            action_content: visible.then(|| "Material follow-through action".into()),
        }
    }

    fn delegation_event(
        driver: &Keys,
        channel: &str,
        thread: &str,
        task: &TaskBinding,
        include_markers: bool,
    ) -> nostr::Event {
        let content = if include_markers {
            format!(
                "Please own this task.\nExpected result: {}\nEvidence locator: {}",
                task.expected_result, task.evidence_locator
            )
        } else {
            "Please own this task.".into()
        };
        EventBuilder::new(Kind::Custom(KIND_STREAM_MESSAGE as u16), content)
            .tags([
                Tag::parse(["h", channel]).unwrap(),
                Tag::parse(["e", thread, "", "reply"]).unwrap(),
                Tag::parse(["p", &task.assignee_pubkey]).unwrap(),
            ])
            .sign_with_keys(driver)
            .unwrap()
    }

    #[test]
    fn adoption_accepts_an_addressed_message_and_derives_a_stable_private_id() {
        let author = Keys::generate();
        let assignee = Keys::generate();
        let other = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let event = EventBuilder::new(
            Kind::Custom(KIND_STREAM_MESSAGE as u16),
            "Please finish the existing mobile release.",
        )
        .tags([
            Tag::parse(["h", channel]).unwrap(),
            Tag::parse(["e", &thread, "", "reply"]).unwrap(),
            Tag::parse(["p", &assignee.public_key().to_hex()]).unwrap(),
            Tag::parse(["p", &other.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&author)
        .unwrap();

        let (parsed_channel, parsed_thread) =
            validate_adopted_source_event(&event, &assignee.public_key()).unwrap();
        assert_eq!(parsed_channel, channel);
        assert_eq!(parsed_thread, thread);
        assert_eq!(
            adopted_schedule_id(&event.id.to_hex()).unwrap(),
            format!("adopt-{}", &event.id.to_hex()[..40])
        );
        assert!(validate_adopted_source_event(&event, &Keys::generate().public_key()).is_err());
    }

    fn task_state_event(
        driver: &Keys,
        channel: &str,
        thread: &str,
        delegation_event_id: &str,
        status: &str,
        created_at: u64,
    ) -> nostr::Event {
        EventBuilder::new(Kind::Custom(KIND_STREAM_MESSAGE as u16), "State changed")
            .tags([
                Tag::parse(["h", channel]).unwrap(),
                Tag::parse(["e", thread, "", "reply"]).unwrap(),
                Tag::parse([
                    "d",
                    format!("{TASK_STATE_PREFIX}{delegation_event_id}").as_str(),
                ])
                .unwrap(),
                Tag::parse(["task-status", status]).unwrap(),
            ])
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(driver)
            .unwrap()
    }

    #[test]
    fn assigned_view_parses_exact_structured_delegations() {
        let driver = Keys::generate();
        let assignee = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let task = TaskBinding {
            assignee_pubkey: assignee.public_key().to_hex(),
            delegation_event_id: "b".repeat(64),
            expected_result: "land the bounded fix".into(),
            evidence_locator: "/workspace/fix-worktree".into(),
        };
        let event = delegation_event(&driver, channel, &thread, &task, true);

        let parsed = parse_assigned_task_event(&event, &task.assignee_pubkey).unwrap();
        assert_eq!(parsed.delegation_event_id, event.id.to_hex());
        assert_eq!(parsed.driver_pubkey, driver.public_key().to_hex());
        assert_eq!(parsed.channel_id, channel);
        assert_eq!(parsed.thread_id, thread);
        assert_eq!(parsed.expected_result, task.expected_result);
        assert_eq!(parsed.evidence_locator, task.evidence_locator);
        assert_eq!(parsed.status, AssignedTaskStatus::Assigned);

        assert!(
            parse_assigned_task_event(&event, &Keys::generate().public_key().to_hex()).is_none()
        );
        let unstructured = delegation_event(&driver, channel, &thread, &task, false);
        assert!(parse_assigned_task_event(&unstructured, &task.assignee_pubkey).is_none());
    }

    #[test]
    fn assigned_view_accepts_only_driver_scoped_lifecycle_state() {
        let driver = Keys::generate();
        let assignee = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let task = TaskBinding {
            assignee_pubkey: assignee.public_key().to_hex(),
            delegation_event_id: "b".repeat(64),
            expected_result: "return the exact candidate".into(),
            evidence_locator: "/workspace/candidate".into(),
        };
        let delegation = delegation_event(&driver, channel, &thread, &task, true);
        let mut parsed = parse_assigned_task_event(&delegation, &task.assignee_pubkey).unwrap();
        let completed = task_state_event(
            &driver,
            channel,
            &thread,
            &delegation.id.to_hex(),
            "completed",
            parsed.delegated_at + 1,
        );
        assert_eq!(
            task_state_reference(&completed),
            Some(delegation.id.to_hex().as_str())
        );
        apply_task_state(&mut parsed, &completed);
        assert_eq!(parsed.status, AssignedTaskStatus::Completed);
        assert!(parsed.status.is_closed());
        assert_eq!(parsed.status_event_id, Some(completed.id.to_hex()));

        let forged = task_state_event(
            &Keys::generate(),
            channel,
            &thread,
            &delegation.id.to_hex(),
            "woken",
            parsed.updated_at + 1,
        );
        apply_task_state(&mut parsed, &forged);
        assert_eq!(parsed.status, AssignedTaskStatus::Completed);
    }

    #[test]
    fn visible_follow_through_actions_carry_read_only_task_state_tags() {
        let task = TaskBinding {
            assignee_pubkey: "a".repeat(64),
            delegation_event_id: "b".repeat(64),
            expected_result: "bounded result".into(),
            evidence_locator: "/workspace/result".into(),
        };
        assert_eq!(
            task_state_tags(&task, ScheduleDecision::Completed),
            vec![
                vec![
                    "d".to_owned(),
                    format!("{TASK_STATE_PREFIX}{}", task.delegation_event_id),
                ],
                vec!["task-status".to_owned(), "completed".to_owned()],
            ]
        );
    }

    fn scoped_event(
        author: &Keys,
        kind: u32,
        channel: &str,
        thread: &str,
        content: &str,
        mentions: &[&str],
        at: DateTime<Utc>,
    ) -> nostr::Event {
        let mut tags = vec![
            Tag::parse(["h", channel]).unwrap(),
            Tag::parse(["e", thread, "", "reply"]).unwrap(),
        ];
        tags.extend(
            mentions
                .iter()
                .map(|pubkey| Tag::parse(["p", *pubkey]).unwrap()),
        );
        EventBuilder::new(Kind::Custom(kind as u16), content)
            .tags(tags)
            .custom_created_at(nostr::Timestamp::from(
                u64::try_from(at.timestamp()).unwrap(),
            ))
            .sign_with_keys(author)
            .unwrap()
    }

    #[test]
    fn unchanged_or_generic_receipt_cannot_keep_work_alive() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(validate_receipt("online:true").is_err());

        let mut schedule = sample_schedule(now);
        schedule.checkpoint = Some(MaterialCheckpoint {
            receipt: format!("git-commit:{}", "a".repeat(40)),
            material_at: canonical_time(now),
        });
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let input = reconciliation(
            ScheduleDecision::Keep,
            &format!("git-commit:{}", "a".repeat(40)),
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        let error = reconcile_schedule(&schedule, &claim, now, &input, true).unwrap_err();
        assert!(error.to_string().contains("unchanged work is not progress"));
    }

    #[test]
    fn stale_newer_receipt_cannot_defer_recovery() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        schedule.checkpoint = Some(MaterialCheckpoint {
            receipt: format!("git-commit:{}", "a".repeat(40)),
            material_at: canonical_time(now - chrono::Duration::minutes(30)),
        });
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let keep = reconciliation(
            ScheduleDecision::Keep,
            &format!("git-commit:{}", "b".repeat(40)),
            now - chrono::Duration::minutes(16),
            Some(now + chrono::Duration::minutes(15)),
        );

        let error = reconcile_schedule(&schedule, &claim, now, &keep, true).unwrap_err();
        assert!(error
            .to_string()
            .contains("task-bound material no more than 15 minutes old"));
    }

    #[test]
    fn changed_receipt_at_freshness_boundary_can_keep_work_alive() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        schedule.checkpoint = Some(MaterialCheckpoint {
            receipt: format!("git-commit:{}", "a".repeat(40)),
            material_at: canonical_time(now - chrono::Duration::minutes(30)),
        });
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let keep = reconciliation(
            ScheduleDecision::Keep,
            &format!("git-commit:{}", "b".repeat(40)),
            now - chrono::Duration::minutes(15),
            Some(now + chrono::Duration::minutes(15)),
        );

        let (kept, idempotent) = reconcile_schedule(&schedule, &claim, now, &keep, true).unwrap();
        assert!(!idempotent);
        assert_eq!(kept.status, ScheduleStatus::Pending);
        assert_eq!(
            kept.checkpoint.unwrap().receipt,
            format!("git-commit:{}", "b".repeat(40))
        );
    }

    #[test]
    fn one_wake_then_redirect_preserves_result_and_changes_only_assignee() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let original = schedule.task.clone().unwrap();
        let first_claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let wake = reconciliation(
            ScheduleDecision::Wake,
            &format!("document-hash:{}", "1".repeat(64)),
            now - chrono::Duration::minutes(5),
            Some(now + chrono::Duration::minutes(15)),
        );
        let (mut woken, _) = reconcile_schedule(&schedule, &first_claim, now, &wake, true).unwrap();
        assert_eq!(woken.phase, Some(FollowThroughPhase::SameOwnerWoken));
        assert_eq!(woken.audit.len(), 1);

        let second_at = now + chrono::Duration::minutes(15);
        let second_claim = claim_schedule(&mut woken, second_at, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let second_wake_input = reconciliation(
            ScheduleDecision::Wake,
            &format!("document-hash:{}", "1".repeat(64)),
            now - chrono::Duration::minutes(5),
            Some(second_at + chrono::Duration::minutes(15)),
        );
        let second_wake =
            reconcile_schedule(&woken, &second_claim, second_at, &second_wake_input, true)
                .unwrap_err();
        assert!(second_wake
            .to_string()
            .contains("next unchanged decision must redirect"));

        let replacement = Keys::generate().public_key().to_hex();
        let mut redirect = reconciliation(
            ScheduleDecision::Redirect,
            &format!("document-hash:{}", "1".repeat(64)),
            now - chrono::Duration::minutes(5),
            Some(second_at + chrono::Duration::minutes(15)),
        );
        redirect.replacement_pubkey = Some(replacement.clone());
        redirect.replacement_delegation_event_id = Some("c".repeat(64));
        redirect.action_event_id = Some("c".repeat(64));
        let (redirected, _) =
            reconcile_schedule(&woken, &second_claim, second_at, &redirect, true).unwrap();
        let task = redirected.task.unwrap();
        assert_eq!(task.assignee_pubkey, replacement);
        assert_eq!(task.delegation_event_id, "c".repeat(64));
        assert_eq!(task.expected_result, original.expected_result);
        assert_eq!(task.evidence_locator, original.evidence_locator);
        assert_eq!(redirected.phase, Some(FollowThroughPhase::Monitoring));
        assert_eq!(redirected.audit.len(), 2);
    }

    #[test]
    fn newer_receipt_after_wake_resets_the_wake_allowance() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        schedule.checkpoint = Some(MaterialCheckpoint {
            receipt: format!("git-commit:{}", "a".repeat(40)),
            material_at: canonical_time(now - chrono::Duration::minutes(5)),
        });
        schedule.phase = Some(FollowThroughPhase::SameOwnerWoken);
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let keep = reconciliation(
            ScheduleDecision::Keep,
            &format!("git-commit:{}", "b".repeat(40)),
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        let (kept, _) = reconcile_schedule(&schedule, &claim, now, &keep, true).unwrap();
        assert_eq!(kept.phase, Some(FollowThroughPhase::Monitoring));
        assert_eq!(
            kept.checkpoint.unwrap().receipt,
            format!("git-commit:{}", "b".repeat(40))
        );
    }

    #[test]
    fn audit_rollover_uses_actual_outer_nip_ae_size_and_preserves_history() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let task = schedule.task.clone().unwrap();
        for index in 0..200_u32 {
            let receipt = format!("document-hash:{index:064x}");
            let claim_token = format!("{index:032x}");
            schedule.audit.push(DecisionAudit {
                claim_token: claim_token.clone(),
                decision: ScheduleDecision::Keep,
                at: canonical_time(now),
                assignee_pubkey: task.assignee_pubkey.clone(),
                delegation_event_id: task.delegation_event_id.clone(),
                receipt: receipt.clone(),
                material_at: canonical_time(now),
                next_due_at: Some(canonical_time(now + chrono::Duration::minutes(15))),
                replacement_pubkey: None,
                replacement_delegation_event_id: None,
                action_event_id: None,
                action_content: None,
            });
            schedule.checkpoint = Some(MaterialCheckpoint {
                receipt,
                material_at: canonical_time(now),
            });
            schedule.last_transition = Some(LastTransition {
                kind: TransitionKind::Rescheduled,
                claim_token,
                at: canonical_time(now),
            });
            let value = schedule_json(&schedule).unwrap();
            if engram_plaintext_len(&schedule_slug(&schedule.id), value)
                > ACTIVE_HEAD_ROLLOVER_BYTES
            {
                break;
            }
        }
        let original_len = schedule.audit.len();
        let (archive, drain_count) = plan_audit_rollover(&schedule)
            .unwrap()
            .expect("head beyond rollover threshold");
        assert!(drain_count > 0 && drain_count < original_len);
        assert_eq!(archive.entries, schedule.audit[..drain_count]);
        let archive_json = archive_value(&archive).unwrap();
        let archive_digest = sha256_hex(archive_json.as_bytes());
        assert!(
            engram_plaintext_len(
                &archive_slug(&schedule.id, archive.sequence, &archive_digest),
                archive_json
            ) <= engram::NIP44_PLAINTEXT_MAX
        );
        assert_eq!(schedule.audit.len(), original_len);
    }

    #[test]
    fn schema_two_cannot_be_read_as_legacy_and_inconsistent_shapes_fail() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = sample_schedule(now);
        let value = serde_json::to_string(&schedule).unwrap();
        assert!(serde_json::from_str::<LegacyScheduleV1>(&value).is_err());

        let mut inconsistent = schedule;
        inconsistent.status = ScheduleStatus::Completed;
        assert!(validate_schedule(&inconsistent)
            .unwrap_err()
            .to_string()
            .contains("completed status and completed phase disagree"));
    }

    #[test]
    fn claimed_legacy_schedule_binds_once_and_lost_output_retry_is_idempotent() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut legacy = sample_legacy_schedule(now);
        let claim = claim_schedule(&mut legacy, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let task = TaskBinding {
            assignee_pubkey: Keys::generate().public_key().to_hex(),
            delegation_event_id: "b".repeat(64),
            expected_result: "reviewed mobile release candidate".into(),
            evidence_locator: "worktree mobile-release".into(),
        };
        let checkpoint = MaterialCheckpoint {
            receipt: format!("git-commit:{}", "2".repeat(40)),
            material_at: canonical_time(now - chrono::Duration::minutes(1)),
        };
        let due_at = canonical_time(now + chrono::Duration::minutes(15));
        let (bound, idempotent) = bind_schedule(
            &legacy,
            &claim,
            now,
            due_at.clone(),
            task.clone(),
            checkpoint.clone(),
        )
        .unwrap();
        assert!(!idempotent);
        assert_eq!(bound.schema, TASK_SCHEMA_VERSION);
        assert_eq!(bound.task.as_ref(), Some(&task));

        let (retried, idempotent) =
            bind_schedule(&bound, &claim, now, due_at, task, checkpoint).unwrap();
        assert!(idempotent);
        assert_eq!(retried, bound);
    }

    #[test]
    fn claimed_legacy_work_can_finish_or_continue_without_a_new_delegation() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut legacy = sample_legacy_schedule(now);
        let claim = claim_schedule(&mut legacy, now, 300).unwrap().unwrap();
        let due_at = canonical_time(now + chrono::Duration::minutes(10));
        let input = || LegacyRescheduleInput {
            due_at: &due_at,
            expected_cause: None,
            action: None,
            check: None,
        };

        assert!(!reschedule_legacy_schedule(&mut legacy, &claim, now, input()).unwrap());
        assert_eq!(legacy.status, ScheduleStatus::Pending);
        assert!(legacy.claim.is_none());
        assert!(reschedule_legacy_schedule(&mut legacy, &claim, now, input()).unwrap());

        let second_claim = claim_schedule(&mut legacy, now + chrono::Duration::minutes(10), 300)
            .unwrap()
            .unwrap();
        assert!(!complete_legacy_schedule(
            &mut legacy,
            &second_claim,
            now + chrono::Duration::minutes(10),
        )
        .unwrap());
        assert_eq!(legacy.status, ScheduleStatus::Completed);
        assert!(legacy.claim.is_none());
        assert!(complete_legacy_schedule(
            &mut legacy,
            &second_claim,
            now + chrono::Duration::minutes(10),
        )
        .unwrap());
    }

    #[test]
    fn legacy_cas_winner_is_idempotent_only_for_the_exact_transition() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut completed = sample_legacy_schedule(now);
        let completion_claim = claim_schedule(&mut completed, now, 300).unwrap().unwrap();
        complete_legacy_schedule(&mut completed, &completion_claim, now).unwrap();
        assert!(legacy_completion_matches(&completed, &completion_claim));
        assert!(!legacy_completion_matches(&completed, &"f".repeat(32)));

        let mut rescheduled = sample_legacy_schedule(now);
        let reschedule_claim = claim_schedule(&mut rescheduled, now, 300).unwrap().unwrap();
        let due_at = canonical_time(now + chrono::Duration::minutes(10));
        let requested = || LegacyRescheduleInput {
            due_at: &due_at,
            expected_cause: None,
            action: None,
            check: None,
        };
        reschedule_legacy_schedule(&mut rescheduled, &reschedule_claim, now, requested()).unwrap();
        assert!(legacy_reschedule_matches(
            &rescheduled,
            &reschedule_claim,
            &requested(),
        ));
        let different_due_at = canonical_time(now + chrono::Duration::minutes(11));
        assert!(!legacy_reschedule_matches(
            &rescheduled,
            &reschedule_claim,
            &LegacyRescheduleInput {
                due_at: &different_due_at,
                expected_cause: None,
                action: None,
                check: None,
            },
        ));

        let mut preserved_candidate = sample_legacy_schedule(now);
        let preserved_claim = claim_schedule(&mut preserved_candidate, now, 300)
            .unwrap()
            .unwrap();
        let claimed_base = preserved_candidate.clone();
        let original_action = preserved_candidate.action.clone();
        reschedule_legacy_schedule(
            &mut preserved_candidate,
            &preserved_claim,
            now,
            LegacyRescheduleInput {
                due_at: &due_at,
                expected_cause: None,
                action: None,
                check: None,
            },
        )
        .unwrap();
        assert_eq!(preserved_candidate.action, original_action);

        let mut different_winner = claimed_base;
        reschedule_legacy_schedule(
            &mut different_winner,
            &preserved_claim,
            now,
            LegacyRescheduleInput {
                due_at: &due_at,
                expected_cause: None,
                action: Some("different concurrent action"),
                check: None,
            },
        )
        .unwrap();
        assert!(!legacy_reschedule_candidate_matches(
            &different_winner,
            &preserved_candidate,
            &preserved_claim,
        ));
    }

    #[test]
    fn task_bound_work_rejects_legacy_transitions() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let claim = claim_schedule(&mut schedule, now, 300).unwrap().unwrap();
        assert!(complete_legacy_schedule(&mut schedule, &claim, now)
            .unwrap_err()
            .to_string()
            .contains("must use `buzz schedules reconcile`"));
        let due_at = canonical_time(now + chrono::Duration::minutes(10));
        assert!(reschedule_legacy_schedule(
            &mut schedule,
            &claim,
            now,
            LegacyRescheduleInput {
                due_at: &due_at,
                expected_cause: None,
                action: None,
                check: None,
            },
        )
        .unwrap_err()
        .to_string()
        .contains("must use `buzz schedules reconcile`"));
    }

    #[test]
    fn receipts_reject_presence_labels_and_malformed_revisions() {
        assert!(validate_receipt("external-job:goji/campaign-1@online-1").is_err());
        assert!(validate_receipt("external-job:worker/task-9@running").is_err());
        assert!(validate_receipt("git-commit:abc123").is_err());
        assert!(validate_receipt(&format!("document-hash:{}", "A".repeat(64))).is_err());
        assert!(validate_receipt("external-job:github/run-123@attempt-02").is_ok());
        assert!(validate_receipt(&format!("worktree-fingerprint:{}", "a".repeat(64))).is_ok());
        assert!(validate_receipt(&format!("source-event:{}", "b".repeat(64))).is_ok());
        assert!(validate_receipt(&format!("source-event:{}", "B".repeat(64))).is_err());
    }

    #[test]
    fn future_material_and_unbounded_next_checks_are_rejected() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let future = reconciliation(
            ScheduleDecision::Keep,
            &format!("document-hash:{}", "2".repeat(64)),
            now + chrono::Duration::seconds(1),
            Some(now + chrono::Duration::minutes(15)),
        );
        assert!(reconcile_schedule(&schedule, &claim, now, &future, true)
            .unwrap_err()
            .to_string()
            .contains("cannot be later"));

        let too_late = reconciliation(
            ScheduleDecision::Keep,
            &format!("document-hash:{}", "2".repeat(64)),
            now,
            Some(now + chrono::Duration::hours(4)),
        );
        assert!(reconcile_schedule(&schedule, &claim, now, &too_late, true)
            .unwrap_err()
            .to_string()
            .contains("10 to 15 minutes"));
        assert!(
            validate_next_due_at(now, &canonical_time(now + chrono::Duration::minutes(20)))
                .is_err()
        );
        assert!(validate_next_due_at(now, &canonical_time(now)).is_err());
    }

    #[test]
    fn delegation_validation_is_driver_and_assignee_neutral_but_exact() {
        let driver = Keys::generate();
        let assignee = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let task = TaskBinding {
            assignee_pubkey: assignee.public_key().to_hex(),
            delegation_event_id: "b".repeat(64),
            expected_result: "a verified customer-comms draft".into(),
            evidence_locator: "Corpus customer-comms note".into(),
        };
        let event = delegation_event(&driver, channel, &thread, &task, true);
        assert!(!event.content.contains("PM"));
        assert!(!event.content.contains("Koder"));
        validate_delegation_event(&event, &driver.public_key(), channel, &thread, &task).unwrap();

        assert!(validate_delegation_event(
            &event,
            &Keys::generate().public_key(),
            channel,
            &thread,
            &task,
        )
        .unwrap_err()
        .to_string()
        .contains("authored"));
        assert!(validate_delegation_event(
            &event,
            &driver.public_key(),
            "049cb6af-a0af-42ee-a25e-c510cf76a59d",
            &thread,
            &task,
        )
        .unwrap_err()
        .to_string()
        .contains("channel"));
        assert!(validate_delegation_event(
            &event,
            &driver.public_key(),
            channel,
            &"c".repeat(64),
            &task,
        )
        .unwrap_err()
        .to_string()
        .contains("thread"));

        let no_markers = delegation_event(&driver, channel, &thread, &task, false);
        assert!(validate_delegation_event(
            &no_markers,
            &driver.public_key(),
            channel,
            &thread,
            &task,
        )
        .unwrap_err()
        .to_string()
        .contains("exactly one"));

        let assignee_receipt = delegation_event(&assignee, channel, &thread, &task, false);
        validate_buzz_receipt_event(&assignee_receipt, channel, &thread, &task.assignee_pubkey)
            .unwrap();
        assert!(
            validate_buzz_receipt_event(&event, channel, &thread, &task.assignee_pubkey,)
                .unwrap_err()
                .to_string()
                .contains("exact assignee")
        );

        let mut wrong_assignee = task.clone();
        wrong_assignee.assignee_pubkey = Keys::generate().public_key().to_hex();
        assert!(validate_delegation_event(
            &event,
            &driver.public_key(),
            channel,
            &thread,
            &wrong_assignee,
        )
        .unwrap_err()
        .to_string()
        .contains("exactly one assignee"));
    }

    #[test]
    fn reconciliation_lost_output_retry_is_exact_and_competing_decision_is_rejected() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let claim = claim_schedule(&mut schedule, now, DEFAULT_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let keep = reconciliation(
            ScheduleDecision::Keep,
            &format!("document-hash:{}", "2".repeat(64)),
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        let (kept, idempotent) = reconcile_schedule(&schedule, &claim, now, &keep, true).unwrap();
        assert!(!idempotent);
        let (retried, idempotent) = reconcile_schedule(&kept, &claim, now, &keep, true).unwrap();
        assert!(idempotent);
        assert_eq!(retried, kept);

        let competing = reconciliation(
            ScheduleDecision::Wake,
            &format!("document-hash:{}", "2".repeat(64)),
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        assert!(reconcile_schedule(&kept, &claim, now, &competing, true)
            .unwrap_err()
            .to_string()
            .contains("different reconciliation decision"));
    }

    #[test]
    fn expired_claim_is_recoverable_but_completed_work_is_not() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut schedule = sample_schedule(now);
        let first = claim_schedule(&mut schedule, now, 60).unwrap().unwrap();
        assert!(!is_due(&schedule, now + chrono::Duration::seconds(59)).unwrap());
        assert!(is_due(&schedule, now + chrono::Duration::seconds(60)).unwrap());
        let wake = reconciliation(
            ScheduleDecision::Wake,
            &format!("document-hash:{}", "1".repeat(64)),
            now - chrono::Duration::minutes(5),
            Some(now + chrono::Duration::minutes(11)),
        );
        assert!(reconcile_schedule(
            &schedule,
            &first,
            now + chrono::Duration::seconds(60),
            &wake,
            true,
        )
        .unwrap_err()
        .to_string()
        .contains("claim lease expired"));
        let second = claim_schedule(&mut schedule, now + chrono::Duration::seconds(60), 60)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn uuid_receipts_have_one_canonical_identity() {
        let uuid = Uuid::parse_str("A0B1C2D3-E4F5-4678-9ABC-DEF012345678").unwrap();
        let canonical = uuid.hyphenated().to_string();
        for raw in [
            uuid.hyphenated().to_string().to_uppercase(),
            uuid.simple().to_string(),
            uuid.braced().to_string(),
        ] {
            assert_eq!(
                validate_receipt(&format!("codex-turn:{raw}")).unwrap(),
                format!("codex-turn:{canonical}"),
            );
        }
        assert_eq!(
            validate_receipt(&format!("cursor-turn:{}", uuid.simple())).unwrap(),
            format!("cursor-turn:{canonical}"),
        );
    }

    #[test]
    fn public_reconcile_contract_requires_exact_visible_action_fields() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let receipt = format!("document-hash:{}", "2".repeat(64));
        let mut keep = reconciliation(
            ScheduleDecision::Keep,
            &receipt,
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        validate_public_reconcile_shape(&keep).unwrap();
        keep.action_content = Some("not silent".into());
        assert!(validate_public_reconcile_shape(&keep)
            .unwrap_err()
            .to_string()
            .contains("must not include --message"));

        let mut wake = reconciliation(
            ScheduleDecision::Wake,
            &receipt,
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        wake.action_content = None;
        assert!(validate_public_reconcile_shape(&wake)
            .unwrap_err()
            .to_string()
            .contains("wake requires --message"));
        wake.action_content = Some("Please continue and report material progress.".into());
        validate_public_reconcile_shape(&wake).unwrap();

        let mut redirect = reconciliation(
            ScheduleDecision::Redirect,
            &receipt,
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        assert!(validate_public_reconcile_shape(&redirect)
            .unwrap_err()
            .to_string()
            .contains("redirect requires --replacement"));
        redirect.replacement_pubkey = Some(Keys::generate().public_key().to_hex());
        redirect.action_content = None;
        assert!(validate_public_reconcile_shape(&redirect)
            .unwrap_err()
            .to_string()
            .contains("redirect requires --message"));
        redirect.action_content = Some(
            "Please take over.\nExpected result: exact result\nEvidence locator: exact locator"
                .into(),
        );
        validate_public_reconcile_shape(&redirect).unwrap();

        let mut complete = reconciliation(ScheduleDecision::Completed, &receipt, now, None);
        complete.action_content = None;
        assert!(validate_public_reconcile_shape(&complete)
            .unwrap_err()
            .to_string()
            .contains("complete requires --message"));
        complete.action_content = Some("Completed with exact evidence.".into());
        validate_public_reconcile_shape(&complete).unwrap();
        complete.due_at = Some(canonical_time(now + chrono::Duration::minutes(15)));
        assert!(validate_public_reconcile_shape(&complete)
            .unwrap_err()
            .to_string()
            .contains("must be omitted"));
    }

    #[test]
    fn buzz_event_receipts_are_material_and_task_scoped() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let assignee = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let valid = scoped_event(
            &assignee,
            KIND_STREAM_MESSAGE,
            channel,
            &thread,
            "Material callback with evidence.",
            &[],
            now,
        );
        validate_buzz_receipt_event(&valid, channel, &thread, &assignee.public_key().to_hex())
            .unwrap();
        assert_eq!(
            require_buzz_event_material_at(&valid, &canonical_time(now)).unwrap(),
            canonical_time(now),
        );
        assert!(require_buzz_event_material_at(
            &valid,
            &canonical_time(now + chrono::Duration::seconds(1)),
        )
        .is_err());

        let wrong_author = scoped_event(
            &Keys::generate(),
            KIND_STREAM_MESSAGE,
            channel,
            &thread,
            "Material callback with evidence.",
            &[],
            now,
        );
        assert!(validate_buzz_receipt_event(
            &wrong_author,
            channel,
            &thread,
            &assignee.public_key().to_hex(),
        )
        .is_err());
        let wrong_channel = scoped_event(
            &assignee,
            KIND_STREAM_MESSAGE,
            "049cb6af-a0af-42ee-a25e-c510cf76a59d",
            &thread,
            "Material callback with evidence.",
            &[],
            now,
        );
        assert!(validate_buzz_receipt_event(
            &wrong_channel,
            channel,
            &thread,
            &assignee.public_key().to_hex(),
        )
        .is_err());
        let wrong_thread = scoped_event(
            &assignee,
            KIND_STREAM_MESSAGE,
            channel,
            &"b".repeat(64),
            "Material callback with evidence.",
            &[],
            now,
        );
        assert!(validate_buzz_receipt_event(
            &wrong_thread,
            channel,
            &thread,
            &assignee.public_key().to_hex(),
        )
        .is_err());
        let wrong_kind = scoped_event(
            &assignee,
            1,
            channel,
            &thread,
            "Material callback with evidence.",
            &[],
            now,
        );
        assert!(validate_buzz_receipt_event(
            &wrong_kind,
            channel,
            &thread,
            &assignee.public_key().to_hex(),
        )
        .is_err());
        let empty = scoped_event(
            &assignee,
            KIND_STREAM_MESSAGE,
            channel,
            &thread,
            "  ",
            &[],
            now,
        );
        assert!(validate_buzz_receipt_event(
            &empty,
            channel,
            &thread,
            &assignee.public_key().to_hex(),
        )
        .is_err());
    }

    #[test]
    fn duplicate_delegation_markers_are_rejected() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let driver = Keys::generate();
        let assignee = Keys::generate();
        let channel = "94b69f8a-59ab-4bd7-a049-e898ae1f624e";
        let thread = "a".repeat(64);
        let task = TaskBinding {
            assignee_pubkey: assignee.public_key().to_hex(),
            delegation_event_id: "b".repeat(64),
            expected_result: "exact result".into(),
            evidence_locator: "exact locator".into(),
        };
        for content in [
            "Expected result: exact result\nExpected result: exact result\nEvidence locator: exact locator",
            "Expected result: exact result\nEvidence locator: exact locator\nEvidence locator: exact locator",
            "Expected result: conflicting result\nExpected result: exact result\nEvidence locator: exact locator",
        ] {
            let event = scoped_event(
                &driver,
                KIND_STREAM_MESSAGE,
                channel,
                &thread,
                content,
                &[&task.assignee_pubkey],
                now,
            );
            assert!(validate_delegation_event(
                &event,
                &driver.public_key(),
                channel,
                &thread,
                &task,
            )
            .unwrap_err()
            .to_string()
            .contains("exactly one"));
        }
    }

    #[test]
    fn exact_retries_survive_wall_clock_advance() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let existing = sample_schedule(now);
        assert!(create_definition_matches(&existing, &existing.clone()));

        let mut legacy = sample_legacy_schedule(now);
        let bind_claim = claim_schedule(&mut legacy, now, 60).unwrap().unwrap();
        let task = existing.task.clone().unwrap();
        let checkpoint = existing.checkpoint.clone().unwrap();
        let due_at = canonical_time(now + chrono::Duration::minutes(15));
        let (bound, _) = bind_schedule(
            &legacy,
            &bind_claim,
            now,
            due_at.clone(),
            task.clone(),
            checkpoint.clone(),
        )
        .unwrap();
        let (_, bind_retry) = bind_schedule(
            &bound,
            &bind_claim,
            now + chrono::Duration::hours(2),
            due_at,
            task,
            checkpoint,
        )
        .unwrap();
        assert!(bind_retry);

        let mut schedule = sample_schedule(now);
        let claim = claim_schedule(&mut schedule, now, 60).unwrap().unwrap();
        let keep = reconciliation(
            ScheduleDecision::Keep,
            &format!("document-hash:{}", "2".repeat(64)),
            now,
            Some(now + chrono::Duration::minutes(15)),
        );
        let (kept, _) = reconcile_schedule(&schedule, &claim, now, &keep, true).unwrap();
        let (_, reconcile_retry) =
            reconcile_schedule(&kept, &claim, now + chrono::Duration::hours(2), &keep, true)
                .unwrap();
        assert!(reconcile_retry);
    }

    #[test]
    fn prepared_action_survives_a_replacement_lease_without_changing_event_id() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let driver = Keys::generate();
        let mut schedule = sample_schedule(now);
        let assignee = schedule.task.as_ref().unwrap().assignee_pubkey.clone();
        let first_claim = claim_schedule(&mut schedule, now, 60).unwrap().unwrap();
        let mut wake = reconciliation(
            ScheduleDecision::Wake,
            &format!("document-hash:{}", "1".repeat(64)),
            now - chrono::Duration::minutes(5),
            Some(now + chrono::Duration::minutes(15)),
        );
        wake.action_content = Some("Please continue and report material progress.".into());
        let event = scoped_event(
            &driver,
            KIND_STREAM_MESSAGE,
            &schedule.channel_id,
            &schedule.thread_id,
            wake.action_content.as_deref().unwrap(),
            &[&assignee],
            now,
        );
        let event_id = event.id.to_hex();
        let (prepared, _) =
            prepare_visible_action(&schedule, &first_claim, now, &wake, event).unwrap();
        let serialized = serialized_schedule(&prepared).unwrap();
        let mut after_restart: Schedule = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            after_restart
                .pending_action
                .as_ref()
                .unwrap()
                .event
                .id
                .to_hex(),
            event_id,
        );

        let second_claim =
            claim_schedule(&mut after_restart, now + chrono::Duration::seconds(60), 60)
                .unwrap()
                .unwrap();
        let stored = after_restart.pending_action.as_ref().unwrap();
        assert!(pending_matches_request(stored, &wake));
        let mut competing = wake.clone();
        competing.decision = ScheduleDecision::Completed;
        competing.due_at = None;
        assert!(!pending_matches_request(stored, &competing));
        assert_eq!(
            input_for_pending(stored).action_event_id,
            Some(event_id.clone())
        );
        assert!(finalize_pending_action(&after_restart, &first_claim).is_err());
        let (finalized, _) = finalize_pending_action(&after_restart, &second_claim).unwrap();
        assert_eq!(
            finalized.audit.last().unwrap().action_event_id,
            Some(event_id)
        );
    }

    #[test]
    fn content_addressed_archives_cannot_collide_at_one_sequence() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = sample_schedule(now);
        let task = schedule.task.as_ref().unwrap();
        let entry = |digit: char| DecisionAudit {
            claim_token: digit.to_string().repeat(32),
            decision: ScheduleDecision::Keep,
            at: canonical_time(now),
            assignee_pubkey: task.assignee_pubkey.clone(),
            delegation_event_id: task.delegation_event_id.clone(),
            receipt: format!("document-hash:{}", digit.to_string().repeat(64)),
            material_at: canonical_time(now),
            next_due_at: Some(canonical_time(now + chrono::Duration::minutes(15))),
            replacement_pubkey: None,
            replacement_delegation_event_id: None,
            action_event_id: None,
            action_content: None,
        };
        let archive_a = AuditArchive {
            schema: 1,
            schedule_id: schedule.id.clone(),
            sequence: 1,
            previous: None,
            entries: vec![entry('1')],
        };
        let archive_b = AuditArchive {
            entries: vec![entry('2')],
            ..archive_a.clone()
        };
        let value_a = archive_value(&archive_a).unwrap();
        let value_b = archive_value(&archive_b).unwrap();
        let slug_a = archive_slug(&archive_a.schedule_id, 1, &sha256_hex(value_a.as_bytes()));
        let slug_b = archive_slug(&archive_b.schedule_id, 1, &sha256_hex(value_b.as_bytes()));
        assert_ne!(slug_a, slug_b);
    }

    #[test]
    fn binding_registry_fences_both_race_directions_and_retries_exactly() {
        let empty = TaskBindingRegistry {
            schema: 1,
            by_delegation: BTreeMap::new(),
            by_schedule: BTreeMap::new(),
        };
        let delegation_a = "a".repeat(64);
        let delegation_b = "b".repeat(64);

        let mut winner = empty.clone();
        assert!(!reserve_binding_in_registry(&mut winner, &delegation_a, "schedule-a").unwrap());
        assert!(reserve_binding_in_registry(&mut winner, &delegation_a, "schedule-a").unwrap());
        assert!(reserve_binding_in_registry(&mut winner, &delegation_a, "schedule-b").is_err());
        assert!(reserve_binding_in_registry(&mut winner, &delegation_b, "schedule-a").is_err());

        let mut same_delegation_racer = empty.clone();
        reserve_binding_in_registry(&mut same_delegation_racer, &delegation_a, "schedule-b")
            .unwrap();
        assert_ne!(winner, same_delegation_racer);
        assert!(reserve_binding_in_registry(&mut winner, &delegation_a, "schedule-b").is_err());

        let mut same_schedule_racer = empty;
        reserve_binding_in_registry(&mut same_schedule_racer, &delegation_b, "schedule-a").unwrap();
        assert_ne!(winner, same_schedule_racer);
        assert!(reserve_binding_in_registry(&mut winner, &delegation_b, "schedule-a").is_err());

        let mut redirected = TaskBindingRegistry {
            schema: 1,
            by_delegation: BTreeMap::from([(delegation_a.clone(), "schedule-a".into())]),
            by_schedule: BTreeMap::from([("schedule-a".into(), delegation_a.clone())]),
        };
        assert!(!advance_binding_in_registry(
            &mut redirected,
            "schedule-a",
            &delegation_a,
            &delegation_b,
        )
        .unwrap());
        assert!(advance_binding_in_registry(
            &mut redirected,
            "schedule-a",
            &delegation_a,
            &delegation_b,
        )
        .unwrap());
        assert_eq!(
            redirected.by_delegation.get(&delegation_a),
            Some(&"schedule-a".into()),
        );
        assert_eq!(
            redirected.by_delegation.get(&delegation_b),
            Some(&"schedule-a".into()),
        );
        assert_eq!(
            redirected.by_schedule.get("schedule-a"),
            Some(&delegation_b),
        );
        assert!(reserve_binding_in_registry(&mut redirected, &delegation_a, "schedule-b").is_err());
        assert!(reserve_binding_in_registry(&mut redirected, &delegation_b, "schedule-b").is_err());

        let mut missing_history = redirected;
        missing_history.by_delegation.remove(&delegation_a);
        assert!(advance_binding_in_registry(
            &mut missing_history,
            "schedule-a",
            &delegation_a,
            &delegation_b,
        )
        .unwrap_err()
        .to_string()
        .contains("lost its historical delegation reservation"));
    }

    #[test]
    fn binding_registry_supplies_only_schedules_missing_from_broad_discovery() {
        let delegation_a = "a".repeat(64);
        let delegation_b = "b".repeat(64);
        let registry = TaskBindingRegistry {
            schema: 1,
            by_delegation: BTreeMap::from([
                (delegation_a.clone(), "schedule-a".into()),
                (delegation_b.clone(), "schedule-b".into()),
            ]),
            by_schedule: BTreeMap::from([
                ("schedule-a".into(), delegation_a),
                ("schedule-b".into(), delegation_b),
            ]),
        };
        let present = HashSet::from(["schedule-a".to_owned(), "legacy-only".to_owned()]);

        assert_eq!(
            missing_registered_schedule_ids(&registry, &present).unwrap(),
            vec!["schedule-b"],
        );
    }

    #[test]
    fn binding_registry_fallback_fails_closed_on_inconsistent_state() {
        let registry = TaskBindingRegistry {
            schema: 1,
            by_delegation: BTreeMap::from([("a".repeat(64), "schedule-a".into())]),
            by_schedule: BTreeMap::new(),
        };

        assert!(missing_registered_schedule_ids(&registry, &HashSet::new())
            .unwrap_err()
            .to_string()
            .contains("inconsistent mappings"));
    }

    #[test]
    fn task_bound_transition_commands_fail_closed() {
        for transition in ["completion", "rescheduling"] {
            assert!(task_bound_transition_error(transition)
                .to_string()
                .contains("must use `buzz schedules reconcile`"));
        }
    }

    #[test]
    fn one_claim_conflict_does_not_discard_an_earlier_success() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let first = sample_schedule(now);
        let mut second = sample_schedule(now);
        second.id = "second-id".into();
        let mut claimed = Vec::new();
        record_claim_write(&mut claimed, first.clone(), Ok("1".repeat(64))).unwrap();
        record_claim_write(
            &mut claimed,
            second,
            Err(CliError::Conflict("simulated losing CAS".into())),
        )
        .unwrap();
        assert_eq!(claimed, vec![(first, "1".repeat(64))]);
    }

    #[test]
    fn two_synthetic_agents_get_distinct_private_schedule_coordinates() {
        let owner = Keys::generate();
        let agent_a = Keys::generate();
        let agent_b = Keys::generate();
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let value = serde_json::to_string(&sample_schedule(now)).unwrap();
        let slug = schedule_slug("same-id");
        let body = Body::Memory {
            slug,
            value: Some(value),
        };
        let event_a = build_event(&agent_a, &owner.public_key(), &body, 1).unwrap();
        let event_b = build_event(&agent_b, &owner.public_key(), &body, 1).unwrap();
        let d_a = event_a
            .tags
            .iter()
            .find(|tag| tag.kind().to_string() == "d")
            .and_then(|tag| tag.content())
            .unwrap();
        let d_b = event_b
            .tags
            .iter()
            .find(|tag| tag.kind().to_string() == "d")
            .and_then(|tag| tag.content())
            .unwrap();
        assert_ne!(d_a, d_b);
        assert_eq!(
            validate_and_decrypt(
                &event_a,
                &agent_a.public_key(),
                &owner.public_key(),
                agent_a.secret_key(),
                &owner.public_key(),
            )
            .unwrap(),
            body
        );
        assert!(validate_and_decrypt(
            &event_a,
            &agent_b.public_key(),
            &owner.public_key(),
            agent_b.secret_key(),
            &owner.public_key(),
        )
        .is_err());
    }
}
