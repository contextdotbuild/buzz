//! Fetch the agent's NIP-AE `core` engram at session creation and render it
//! into a prompt section.
//!
//! Scope per Tyler's spec:
//! - Fire one synchronous query for the core head when a *new* session is born.
//! - If a body is found, emit `[Agent Memory — core]\n<profile>`.
//! - If no body is found, emit an onboarding nudge so the agent learns how
//!   to set its own core.
//! - On any *error* (transport, parse), log and emit nothing. We must not
//!   mistake a relay outage for "no core" — that would invite the agent to
//!   overwrite real, just-unreachable memory with a fresh profile.
//! - Either way, session creation is never blocked.

use std::collections::{HashMap, HashSet};

use buzz_core::engram::{conversation_key, d_tag, select_head, validate_and_decrypt, Body};
use buzz_core::kind::KIND_AGENT_ENGRAM;
use chrono::{DateTime, Utc};
use nostr::{Event, EventId, Keys, PublicKey};
use serde::Deserialize;

use crate::relay::RestClient;

/// Section header rendered into the prompt.
const SECTION_LABEL: &str = "Agent Memory — core";
const FOLLOW_THROUGH_PREFIX: &str = "mem/buzz-follow-through/";
const MEMORY_PAGE_LIMIT: usize = 1_000;
const MAX_MEMORY_PAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FollowThroughStatus {
    Pending,
    Claimed,
    Completed,
}

#[derive(Debug, Deserialize)]
struct FollowThroughClaim {
    lease_expires_at: String,
}

#[derive(Debug, Deserialize)]
struct FollowThroughHead {
    due_at: String,
    status: FollowThroughStatus,
    claim: Option<FollowThroughClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MemoryCursor {
    until: u64,
    before_id: EventId,
}

/// Onboarding nudge for new agents with no core yet.
///
/// Wording is from Tyler's brief: "No core memory found. Use `buzz mem`
/// to create a core memory. Ask your user about yourself."
pub const ONBOARDING_NUDGE: &str = "No core memory found. \
Use `buzz mem set core \"…\"` to create one (it will hold your identity, \
rules, and goals across sessions). Ask your user about yourself.";

/// Build the rendered prompt section for the agent's core.
///
/// Returns:
/// - `Some(profile_section)` when a valid core exists,
/// - `Some(nudge_section)` when the relay confirmed absence,
/// - `None` when the fetch failed (transport, parse, decrypt) — the caller
///   should inject no section in that case so the agent doesn't conclude
///   memory is empty.
pub async fn build_core_section(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Option<String> {
    match fetch_core_body(rest, agent_keys, owner).await {
        Ok(Some(profile)) => Some(format!("[{SECTION_LABEL}]\n{profile}")),
        Ok(None) => Some(format!("[{SECTION_LABEL}]\n{ONBOARDING_NUDGE}")),
        Err(reason) => {
            tracing::warn!(
                target: "engram::core",
                "core fetch failed: {reason} — emitting no section to avoid \
                 confusing a relay outage with an absent core"
            );
            None
        }
    }
}

/// Read the agent's private follow-through heads and return the earliest time
/// at which one can be claimed.
///
/// The fixed heartbeat remains the fallback if this best-effort refresh fails.
/// A claimed item becomes eligible at the later of its due time and lease
/// expiry; completed items are excluded.
pub async fn fetch_earliest_follow_through_due(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Result<Option<DateTime<Utc>>, String> {
    let events = query_all_agent_engrams(rest, &agent_keys.public_key(), owner).await?;
    let mut groups: HashMap<String, Vec<(Event, Body)>> = HashMap::new();
    for event in events {
        if event.verify().is_err() {
            continue;
        }
        let Some(d_value) = event
            .tags
            .iter()
            .find(|tag| tag.kind().to_string() == "d")
            .and_then(|tag| tag.content())
            .map(str::to_owned)
        else {
            continue;
        };
        let body = match validate_and_decrypt(
            &event,
            &agent_keys.public_key(),
            owner,
            agent_keys.secret_key(),
            owner,
        ) {
            Ok(body) => body,
            Err(_) => continue,
        };
        groups.entry(d_value).or_default().push((event, body));
    }

    let mut earliest: Option<DateTime<Utc>> = None;
    for members in groups.into_values() {
        let Some(head) = select_head(members.iter().map(|(event, _)| event.clone())) else {
            continue;
        };
        let Some((_, body)) = members.into_iter().find(|(event, _)| event.id == head.id) else {
            continue;
        };
        let Body::Memory {
            slug,
            value: Some(value),
        } = body
        else {
            continue;
        };
        if !slug.starts_with(FOLLOW_THROUGH_PREFIX) {
            continue;
        }
        match follow_through_claim_at(&value) {
            Ok(Some(claim_at)) => {
                earliest = Some(earliest.map_or(claim_at, |current| current.min(claim_at)));
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    slug,
                    "follow-through due refresh skipped invalid head: {error}"
                );
            }
        }
    }
    Ok(earliest)
}

fn follow_through_claim_at(value: &str) -> Result<Option<DateTime<Utc>>, String> {
    let head: FollowThroughHead =
        serde_json::from_str(value).map_err(|error| format!("invalid schedule JSON: {error}"))?;
    if matches!(head.status, FollowThroughStatus::Completed) {
        return Ok(None);
    }
    let due_at = parse_schedule_time(&head.due_at, "due_at")?;
    match (head.status, head.claim) {
        (FollowThroughStatus::Pending, None) => Ok(Some(due_at)),
        (FollowThroughStatus::Claimed, Some(claim)) => {
            let lease_expires_at =
                parse_schedule_time(&claim.lease_expires_at, "claim.lease_expires_at")?;
            Ok(Some(due_at.max(lease_expires_at)))
        }
        (FollowThroughStatus::Pending, Some(_)) => {
            Err("pending schedule unexpectedly retains a claim".into())
        }
        (FollowThroughStatus::Claimed, None) => Err("claimed schedule has no lease".into()),
        (FollowThroughStatus::Completed, _) => Ok(None),
    }
}

fn parse_schedule_time(raw: &str, field: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| format!("invalid {field}: {error}"))
}

async fn query_all_agent_engrams(
    rest: &RestClient,
    agent: &PublicKey,
    owner: &PublicKey,
) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
    let mut seen_events = HashSet::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor: Option<MemoryCursor> = None;

    for _ in 0..MAX_MEMORY_PAGES {
        let mut filter = serde_json::json!({
            "kinds": [KIND_AGENT_ENGRAM],
            "authors": [agent.to_hex()],
            "#p": [owner.to_hex()],
            "limit": MEMORY_PAGE_LIMIT,
        });
        if let Some(cursor) = &cursor {
            filter["until"] = serde_json::json!(cursor.until);
            filter["before_id"] = serde_json::json!(cursor.before_id.to_hex());
        }
        let value = rest
            .query_raw(&serde_json::json!([filter]))
            .await
            .map_err(|error| format!("relay memory query failed: {error}"))?;
        let page: Vec<Event> = serde_json::from_value(value)
            .map_err(|error| format!("relay memory query returned invalid events: {error}"))?;
        let page_len = page.len();
        let next_cursor = page.last().map(|event| MemoryCursor {
            until: event.created_at.as_secs(),
            before_id: event.id,
        });
        let before_len = events.len();
        for event in page {
            if seen_events.insert(event.id) {
                events.push(event);
            }
        }
        if page_len < MEMORY_PAGE_LIMIT {
            return Ok(events);
        }
        let Some(next_cursor) = next_cursor else {
            return Ok(events);
        };
        if events.len() == before_len || !seen_cursors.insert(next_cursor.clone()) {
            return Err(
                "relay memory pagination made no progress; composite cursor support is required"
                    .into(),
            );
        }
        cursor = Some(next_cursor);
    }

    Err(format!(
        "relay memory query exceeded the bounded {MAX_MEMORY_PAGES}-page traversal"
    ))
}

/// Query the relay for the core head and decode it. Returns:
/// - `Ok(Some(profile))` if a valid core body was found,
/// - `Ok(None)` only if the relay confirmed absence (empty result set),
/// - `Err(reason)` if the relay returned candidates we could not parse,
///   verify, or decrypt — those are NOT treated as absence (would let an
///   unreadable but real core be silently overwritten by the onboarding nudge),
/// - `Err` for transport / parse errors.
async fn fetch_core_body(
    rest: &RestClient,
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Result<Option<String>, String> {
    let k_c = conversation_key(agent_keys.secret_key(), owner);
    let d = d_tag(&k_c, buzz_core::engram::CORE_SLUG);

    let filter = nostr::Filter::new()
        .kind(nostr::Kind::Custom(KIND_AGENT_ENGRAM as u16))
        .author(agent_keys.public_key())
        .custom_tags(nostr::SingleLetterTag::lowercase(nostr::Alphabet::D), [d])
        .custom_tags(
            nostr::SingleLetterTag::lowercase(nostr::Alphabet::P),
            [owner.to_hex()],
        )
        .limit(16);

    let value = rest
        .query(&[filter])
        .await
        .map_err(|e| format!("relay query failed: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "relay query returned non-array".to_string())?;
    decode_core_body(arr, agent_keys, owner)
}

/// Pure decoder: given the relay's JSON array, decide whether we have a
/// readable core, confirmed absence, or an ambiguous unreadable-state.
///
/// - Empty array → `Ok(None)` (confirmed absence; caller renders the nudge).
/// - At least one event decrypts → use the winning head's body.
///   * Body::Core → `Ok(Some(profile))`
///   * Body::Tombstone or unexpected shape → `Ok(None)` (treat as absent).
/// - Non-empty array but nothing decrypts → `Err` (fail closed; caller
///   emits no section, so the agent does not assume memory is empty and
///   try to overwrite a real-but-unreadable core).
fn decode_core_body(
    arr: &[serde_json::Value],
    agent_keys: &Keys,
    owner: &PublicKey,
) -> Result<Option<String>, String> {
    if arr.is_empty() {
        return Ok(None);
    }
    let mut valid_with_body: Vec<(Event, Body)> = Vec::with_capacity(arr.len());
    let mut candidates_seen = 0usize;
    let mut last_decrypt_err: Option<String> = None;
    for ev_json in arr {
        let event: Event = match serde_json::from_value(ev_json.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if event.verify().is_err() {
            continue;
        }
        candidates_seen += 1;
        match validate_and_decrypt(
            &event,
            &agent_keys.public_key(),
            owner,
            agent_keys.secret_key(),
            owner,
        ) {
            Ok(body) => valid_with_body.push((event, body)),
            Err(e) => {
                last_decrypt_err = Some(e.to_string());
                continue;
            }
        }
    }
    if valid_with_body.is_empty() {
        if candidates_seen > 0 {
            return Err(format!(
                "{candidates_seen} core candidate(s) returned but none decryptable                  (last error: {})",
                last_decrypt_err.as_deref().unwrap_or("unknown")
            ));
        }
        return Err(
            "relay returned core candidate(s) that could not be parsed or verified".to_string(),
        );
    }
    let events: Vec<Event> = valid_with_body.iter().map(|(e, _)| e.clone()).collect();
    // `select_head` returns `None` only on an empty iterator, which we
    // ruled out above.
    let Some(head) = select_head(events) else {
        return Ok(None);
    };
    let head_id = head.id;
    let body = valid_with_body
        .into_iter()
        .find(|(e, _)| e.id == head_id)
        .map(|(_, b)| b);
    match body {
        Some(Body::Core { profile }) => Ok(Some(profile)),
        // A tombstone or unexpectedly-shaped head means "no usable core."
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::engram::{build_event, Body};
    use serde_json::json;

    /// Empty array → confirmed absence → Ok(None), so the caller emits the
    /// onboarding nudge. This is the only path that maps to "no core."
    #[test]
    fn decode_empty_array_is_confirmed_absence() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let out = decode_core_body(&[], &agent, &owner.public_key()).unwrap();
        assert_eq!(out, None);
    }

    /// Happy path: a real, decryptable core event yields the profile.
    #[test]
    fn decode_valid_core_returns_profile() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let body = Body::Core {
            profile: "I am Sami.".to_string(),
        };
        let ev = build_event(&agent, &owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let out = decode_core_body(&arr, &agent, &owner.public_key()).unwrap();
        assert_eq!(out.as_deref(), Some("I am Sami."));
    }

    /// Regression: when the relay returns a kind:30174 event addressed to
    /// this agent that we cannot decrypt (here: encrypted to a *different*
    /// owner's key, so the MAC fails for this agent↔owner pair), we MUST
    /// return Err and NOT Ok(None). Returning Ok(None) would cause the
    /// harness to emit the onboarding nudge, inviting the agent to overwrite
    /// a real-but-unreadable core.
    #[test]
    fn decode_undecryptable_candidate_is_err_not_absent() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let wrong_owner = Keys::generate();
        // Build an engram encrypted to wrong_owner (not owner). It will pass
        // sig verification but fail MAC/decrypt for the agent↔owner pair.
        let body = Body::Core {
            profile: "secret".to_string(),
        };
        let ev = build_event(&agent, &wrong_owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let result = decode_core_body(&arr, &agent, &owner.public_key());
        assert!(result.is_err(), "expected Err, got: {result:?}");
        let msg = result.unwrap_err();
        assert!(msg.contains("decryptable"), "got: {msg}");
    }

    /// An unexpectedly-shaped head (here: a Memory body in what was supposed
    /// to be the core slot) is a legitimate, decryptable "no usable core" —
    /// Ok(None). Real `rm core` is refused at the CLI, so this is a defensive
    /// branch for malformed data on the wire.
    #[test]
    fn decode_non_core_body_is_absent() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let body = Body::Memory {
            slug: "mem/x".to_string(),
            value: None,
        };
        let ev = build_event(&agent, &owner.public_key(), &body, 1_700_000_000).unwrap();
        let arr = vec![serde_json::to_value(&ev).unwrap()];
        let out = decode_core_body(&arr, &agent, &owner.public_key()).unwrap();
        assert_eq!(out, None);
    }

    /// Non-empty array with only garbage entries (not even parseable as
    /// events) is also treated as a fetch error, not absence.
    #[test]
    fn decode_unparseable_candidates_is_err() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let arr = vec![json!({"not": "an event"}), json!("garbage")];
        let result = decode_core_body(&arr, &agent, &owner.public_key());
        assert!(result.is_err(), "expected Err, got: {result:?}");
    }

    #[test]
    fn follow_through_claim_time_respects_status_and_lease() {
        let pending = serde_json::json!({
            "due_at": "2026-08-28T15:18:00Z",
            "status": "pending",
            "claim": null,
        });
        assert_eq!(
            follow_through_claim_at(&pending.to_string())
                .unwrap()
                .unwrap()
                .to_rfc3339(),
            "2026-08-28T15:18:00+00:00",
        );

        let claimed = serde_json::json!({
            "due_at": "2026-08-28T15:18:00Z",
            "status": "claimed",
            "claim": { "lease_expires_at": "2026-08-28T15:24:00Z" },
        });
        assert_eq!(
            follow_through_claim_at(&claimed.to_string())
                .unwrap()
                .unwrap()
                .to_rfc3339(),
            "2026-08-28T15:24:00+00:00",
        );

        let completed = serde_json::json!({
            "due_at": "2026-08-28T15:18:00Z",
            "status": "completed",
            "claim": null,
        });
        assert!(follow_through_claim_at(&completed.to_string())
            .unwrap()
            .is_none());
    }
}
