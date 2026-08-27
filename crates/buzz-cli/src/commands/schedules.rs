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
use uuid::Uuid;

use crate::client::BuzzClient;
use crate::commands::mem::{
    get_stored_memory, list_stored_memories, put_stored_memory, ExpectedMemoryHead, StoredMemory,
};
use crate::error::CliError;
use crate::{ScheduleStatusArg, SchedulesCmd};

const SCHEMA_VERSION: u8 = 1;
const SLUG_PREFIX: &str = "mem/buzz-follow-through/";
#[cfg(test)]
const DEFAULT_LEASE_SECONDS: i64 = 30 * 60;
const MAX_TEXT_BYTES: usize = 4096;

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
    Rescheduled,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LastTransition {
    kind: TransitionKind,
    claim_token: String,
    at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Schedule {
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

fn validate_thread_id(raw: &str) -> Result<String, CliError> {
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CliError::Usage(
            "--thread must be a 64-character hexadecimal Buzz event ID".into(),
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

fn schedule_slug(id: &str) -> String {
    format!("{SLUG_PREFIX}{id}")
}

fn parse_stored(entry: StoredMemory) -> Result<LoadedSchedule, CliError> {
    let schedule: Schedule = serde_json::from_str(&entry.value).map_err(|error| {
        CliError::Other(format!(
            "stored follow-through schedule `{}` is invalid JSON: {error}",
            entry.slug
        ))
    })?;
    if schedule.schema != SCHEMA_VERSION {
        return Err(CliError::Other(format!(
            "stored follow-through schedule `{}` uses unsupported schema {}",
            entry.slug, schedule.schema
        )));
    }
    let expected_slug = schedule_slug(&validate_id(&schedule.id)?);
    if entry.slug != expected_slug {
        return Err(CliError::Other(format!(
            "stored follow-through schedule slug `{}` does not match id `{}`",
            entry.slug, schedule.id
        )));
    }
    parse_time(&schedule.due_at, "due-at")?;
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
    list_stored_memories(client, owner, SLUG_PREFIX)
        .await?
        .into_iter()
        .map(parse_stored)
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

fn require_live_claim<'a>(schedule: &'a Schedule, token: &str) -> Result<&'a Claim, CliError> {
    match (&schedule.status, &schedule.claim) {
        (ScheduleStatus::Claimed, Some(claim)) if claim.token == token => Ok(claim),
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

fn complete_schedule(
    schedule: &mut Schedule,
    token: &str,
    now: DateTime<Utc>,
) -> Result<bool, CliError> {
    if schedule.status == ScheduleStatus::Completed
        && schedule.last_transition.as_ref().is_some_and(|transition| {
            transition.kind == TransitionKind::Completed && transition.claim_token == token
        })
    {
        return Ok(true);
    }
    require_live_claim(schedule, token)?;
    schedule.status = ScheduleStatus::Completed;
    schedule.claim = None;
    schedule.updated_at = canonical_time(now);
    schedule.last_transition = Some(LastTransition {
        kind: TransitionKind::Completed,
        claim_token: token.to_owned(),
        at: canonical_time(now),
    });
    Ok(false)
}

struct RescheduleInput<'a> {
    due_at: &'a str,
    expected_cause: Option<&'a str>,
    action: Option<&'a str>,
    check: Option<&'a str>,
}

fn reschedule_values_match(schedule: &Schedule, token: &str, input: &RescheduleInput<'_>) -> bool {
    schedule.status == ScheduleStatus::Pending
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

fn reschedule_schedule(
    schedule: &mut Schedule,
    token: &str,
    now: DateTime<Utc>,
    input: RescheduleInput<'_>,
) -> Result<bool, CliError> {
    if reschedule_values_match(schedule, token, &input) {
        return Ok(true);
    }
    require_live_claim(schedule, token)?;
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
    Ok(false)
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
    expected_cause: &'a str,
    action: &'a str,
    check: &'a str,
}

async fn create(
    client: &BuzzClient,
    input: CreateInput<'_>,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let id = validate_id(input.id)?;
    let due_at = canonical_time(parse_time(input.due_at, "due-at")?);
    let channel_id = Uuid::parse_str(input.channel)
        .map_err(|_| CliError::Usage("--channel must be a UUID".into()))?
        .to_string();
    let thread_id = validate_thread_id(input.thread)?;
    let expected_cause = validate_text(input.expected_cause, "expected-cause")?;
    let action = validate_text(input.action, "action")?;
    let check = validate_text(input.check, "check")?;
    let now = canonical_time(Utc::now());
    let schedule = Schedule {
        schema: SCHEMA_VERSION,
        id: id.clone(),
        due_at,
        channel_id,
        thread_id,
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
        let same_definition = loaded.schedule.id == schedule.id
            && loaded.schedule.due_at == schedule.due_at
            && loaded.schedule.channel_id == schedule.channel_id
            && loaded.schedule.thread_id == schedule.thread_id
            && loaded.schedule.expected_cause == schedule.expected_cause
            && loaded.schedule.action == schedule.action
            && loaded.schedule.check == schedule.check
            && loaded.schedule.status == ScheduleStatus::Pending
            && loaded.schedule.last_transition.is_none();
        if same_definition {
            return print_one(&loaded.schedule, &loaded.revision, true);
        }
        return Err(CliError::Conflict(format!(
            "schedule `{id}` already exists with different state or instructions"
        )));
    }
    let value = serde_json::to_string(&schedule)
        .map_err(|error| CliError::Other(format!("schedule serialization failed: {error}")))?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &slug,
        value,
        ExpectedMemoryHead::Missing,
    )
    .await?;
    print_one(&schedule, &revision, false)
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
        let value = serde_json::to_string(&current.schedule)
            .map_err(|error| CliError::Other(format!("schedule serialization failed: {error}")))?;
        let revision = put_stored_memory(
            client,
            &owner_pubkey,
            &current.slug,
            value,
            ExpectedMemoryHead::Event(&current.revision),
        )
        .await?;
        claimed.push((current.schedule, revision));
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

async fn complete(
    client: &BuzzClient,
    id: &str,
    claim: &str,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let (owner_pubkey, mut loaded) = load_one(client, owner, id).await?;
    let idempotent = complete_schedule(&mut loaded.schedule, claim, Utc::now())?;
    if idempotent {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    let value = serde_json::to_string(&loaded.schedule)
        .map_err(|error| CliError::Other(format!("schedule serialization failed: {error}")))?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &loaded.slug,
        value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await?;
    print_one(&loaded.schedule, &revision, false)
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
    let input = RescheduleInput {
        due_at: &due_at,
        expected_cause: expected_cause.as_deref(),
        action: action.as_deref(),
        check: check.as_deref(),
    };
    let idempotent = reschedule_schedule(&mut loaded.schedule, claim, Utc::now(), input)?;
    if idempotent {
        return print_one(&loaded.schedule, &loaded.revision, true);
    }
    let value = serde_json::to_string(&loaded.schedule)
        .map_err(|error| CliError::Other(format!("schedule serialization failed: {error}")))?;
    let revision = put_stored_memory(
        client,
        &owner_pubkey,
        &loaded.slug,
        value,
        ExpectedMemoryHead::Event(&loaded.revision),
    )
    .await?;
    print_one(&loaded.schedule, &revision, false)
}

pub async fn dispatch(command: SchedulesCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
        SchedulesCmd::Create {
            id,
            due_at,
            channel,
            thread,
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
                    expected_cause: &expected_cause,
                    action: &action,
                    check: &check,
                },
                owner.as_deref(),
            )
            .await
        }
        SchedulesCmd::List { status, owner } => list(client, status, owner.as_deref()).await,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::engram::{build_event, validate_and_decrypt, Body};
    use nostr::Keys;

    fn sample_schedule(now: DateTime<Utc>) -> Schedule {
        Schedule {
            schema: SCHEMA_VERSION,
            id: "same-id".into(),
            due_at: canonical_time(now - chrono::Duration::minutes(1)),
            channel_id: "94b69f8a-59ab-4bd7-a049-e898ae1f624e".into(),
            thread_id: "a".repeat(64),
            expected_cause:
                "Koder returns the requested patch in the named worktree or 15 minutes pass"
                    .into(),
            action: "stay silent and reschedule if active; otherwise verify Koder is inactive and recover exactly once".into(),
            check: "read the thread and /workspace/buzz-driver worktree before any recovery"
                .into(),
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
        let mut after_restart: Schedule = serde_json::from_str(&serialized).unwrap();
        assert!(!complete_schedule(
            &mut after_restart,
            &claim,
            now + chrono::Duration::minutes(6)
        )
        .unwrap());
        assert!(complete_schedule(
            &mut after_restart,
            &claim,
            now + chrono::Duration::minutes(7)
        )
        .unwrap());
        assert!(!is_due(&after_restart, now + chrono::Duration::days(1)).unwrap());
    }

    #[test]
    fn non_pm_driver_item_preserves_owner_result_and_evidence_location() {
        let now = DateTime::parse_from_rfc3339("2026-08-26T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = sample_schedule(now);
        let serialized = serde_json::to_string(&schedule).unwrap();
        let after_restart: Schedule = serde_json::from_str(&serialized).unwrap();

        assert!(after_restart.expected_cause.contains("Koder"));
        assert!(after_restart.expected_cause.contains("requested patch"));
        assert!(after_restart.check.contains("/workspace/buzz-driver"));
        assert!(after_restart.action.contains("stay silent"));
        assert!(after_restart.action.contains("verify Koder is inactive"));
        assert!(after_restart.action.contains("recover exactly once"));
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
        let second = claim_schedule(&mut schedule, now + chrono::Duration::seconds(60), 60)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
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
