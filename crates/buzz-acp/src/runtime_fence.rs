//! Durable single-runtime fencing for one managed Buzz agent identity.
//!
//! The desktop can accidentally launch the same agent key on more than one
//! machine. Relay delivery is fan-out, so process-local event deduplication
//! cannot prevent both processes from answering the same foreground event.
//! This module stores one short encrypted lease in the agent/owner NIP-AE
//! namespace. Atomic `expected-revision` replacement makes exactly one runtime
//! authoritative without changing the agent's public key or constraining any
//! other agent identity.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use buzz_core::engram::{self, conversation_key, d_tag, select_head, validate_and_decrypt, Body};
use buzz_core::kind::KIND_AGENT_ENGRAM;
use nostr::{Event, Keys, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::relay::RestClient;

const RUNTIME_FENCE_SLUG: &str = "mem/buzz-runtime-fence";
const RUNTIME_FENCE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const RUNTIME_FENCE_LEASE_DURATION_SECS: u64 = 90;
/// Renewal must finish this long before the durable lease expires. If it
/// cannot, the runtime revokes local workers immediately while the old lease
/// still prevents a replacement from taking over.
const RUNTIME_FENCE_SHUTDOWN_MARGIN_SECS: u64 = 30;
const RUNTIME_FENCE_CAS_ATTEMPTS: usize = 3;
/// Delay between renewal retries after a transient relay failure. Retries
/// continue until the safe shutdown deadline, so a multi-second relay blip
/// no longer kills a healthy runtime while its lease is still valid.
const RUNTIME_FENCE_RENEW_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RuntimeFenceLease {
    schema: u8,
    runtime_id: String,
    acquired_at: u64,
    lease_expires_at: u64,
}

#[derive(Clone, Debug)]
struct RuntimeFenceHead {
    event: Event,
    lease: RuntimeFenceLease,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeFenceGuard {
    runtime_id: String,
    owner: PublicKey,
    lease_expires_at: u64,
    fail_closed_at: tokio::time::Instant,
    revocation: RuntimeRevocation,
}

/// Process-local authority shared by the fence monitor and every ACP worker.
///
/// The relay event loop can spend an unbounded amount of time in an unrelated
/// metadata/access request. Fence loss therefore cannot rely on that loop
/// receiving a channel message before the durable lease expires. The monitor
/// revokes this object directly: every registered worker process group is
/// killed synchronously, and workers registered after revocation are rejected
/// immediately.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeRevocation {
    inner: Arc<Mutex<RuntimeRevocationState>>,
}

#[derive(Debug, Default)]
struct RuntimeRevocationState {
    revoked: bool,
    process_groups: HashSet<u32>,
}

#[derive(Debug)]
pub(crate) struct RuntimeProcessRegistration {
    revocation: RuntimeRevocation,
    process_group: u32,
}

impl RuntimeProcessRegistration {
    pub(crate) fn revocation(&self) -> RuntimeRevocation {
        self.revocation.clone()
    }
}

impl RuntimeRevocation {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeRevocationState::default())),
        }
    }

    pub(crate) fn register(&self, process_group: u32) -> RuntimeProcessRegistration {
        let revoked = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.revoked {
                true
            } else {
                state.process_groups.insert(process_group);
                false
            }
        };
        if revoked {
            let _ = crate::acp::kill_process_group(process_group);
        }
        RuntimeProcessRegistration {
            revocation: self.clone(),
            process_group,
        }
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revoked
    }

    #[cfg(test)]
    pub(crate) fn registered_process_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .process_groups
            .len()
    }

    /// Fail closed without waiting for the relay event loop to observe loss.
    pub(crate) fn revoke(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.revoked {
            return;
        }
        state.revoked = true;
        for process_group in &state.process_groups {
            let _ = crate::acp::kill_process_group(*process_group);
        }
    }

    fn unregister(&self, process_group: u32) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .process_groups
            .remove(&process_group);
    }
}

impl Drop for RuntimeProcessRegistration {
    fn drop(&mut self) {
        self.revocation.unregister(self.process_group);
    }
}

impl RuntimeFenceGuard {
    pub(crate) fn revocation(&self) -> RuntimeRevocation {
        self.revocation.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExpectedFenceHead {
    Missing,
    Event(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AcquisitionDecision {
    Write {
        lease: RuntimeFenceLease,
        expected: ExpectedFenceHead,
    },
    HeldByOther {
        runtime_id: String,
        lease_expires_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenewalDecision {
    Write {
        lease: RuntimeFenceLease,
        expected_revision: String,
    },
    Lost,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn runtime_is_authorized(lease: Option<&RuntimeFenceLease>, runtime_id: &str, now: u64) -> bool {
    matches!(
        lease,
        Some(lease) if lease.runtime_id == runtime_id && lease.lease_expires_at > now
    )
}

fn choose_acquisition(
    current: Option<(&RuntimeFenceLease, &str)>,
    runtime_id: &str,
    now: u64,
) -> AcquisitionDecision {
    if let Some((lease, _)) = current {
        if lease.runtime_id != runtime_id && lease.lease_expires_at > now {
            return AcquisitionDecision::HeldByOther {
                runtime_id: lease.runtime_id.clone(),
                lease_expires_at: lease.lease_expires_at,
            };
        }
    }

    let (acquired_at, expected) = match current {
        Some((lease, revision)) if lease.runtime_id == runtime_id => (
            lease.acquired_at,
            ExpectedFenceHead::Event(revision.to_string()),
        ),
        Some((_, revision)) => (now, ExpectedFenceHead::Event(revision.to_string())),
        None => (now, ExpectedFenceHead::Missing),
    };
    AcquisitionDecision::Write {
        lease: RuntimeFenceLease {
            schema: 1,
            runtime_id: runtime_id.to_string(),
            acquired_at,
            lease_expires_at: now.saturating_add(RUNTIME_FENCE_LEASE_DURATION_SECS),
        },
        expected,
    }
}

fn choose_renewal(
    current: Option<(&RuntimeFenceLease, &str)>,
    runtime_id: &str,
    now: u64,
) -> RenewalDecision {
    let Some((lease, revision)) = current else {
        return RenewalDecision::Lost;
    };
    if !runtime_is_authorized(Some(lease), runtime_id, now) {
        return RenewalDecision::Lost;
    }
    RenewalDecision::Write {
        lease: RuntimeFenceLease {
            schema: 1,
            runtime_id: runtime_id.to_string(),
            acquired_at: lease.acquired_at,
            lease_expires_at: now.saturating_add(RUNTIME_FENCE_LEASE_DURATION_SECS),
        },
        expected_revision: revision.to_string(),
    }
}

fn runtime_fence_d_tag(keys: &Keys, owner: &PublicKey) -> String {
    let conversation_key = conversation_key(keys.secret_key(), owner);
    d_tag(&conversation_key, RUNTIME_FENCE_SLUG)
}

fn renewal_deadline(
    lease_expires_at: u64,
    wall_now: u64,
    monotonic_now: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let safe_until = lease_expires_at.saturating_sub(RUNTIME_FENCE_SHUTDOWN_MARGIN_SECS);
    (safe_until > wall_now)
        .then(|| monotonic_now + Duration::from_secs(safe_until.saturating_sub(wall_now)))
}

async fn fetch_head(rest: &RestClient, owner: &PublicKey) -> Result<Option<RuntimeFenceHead>> {
    let agent = rest.keys.public_key();
    let d = runtime_fence_d_tag(&rest.keys, owner);
    let response = rest
        .query_raw(&json!([{
            "kinds": [KIND_AGENT_ENGRAM],
            "authors": [agent.to_hex()],
            "#d": [d],
            "#p": [owner.to_hex()],
            "limit": 16,
        }]))
        .await
        .map_err(|error| anyhow!("runtime fence query failed: {error}"))?;
    let events = response
        .as_array()
        .ok_or_else(|| anyhow!("runtime fence query returned a non-array response"))?;

    let mut valid = Vec::new();
    for value in events {
        let Ok(event) = serde_json::from_value::<Event>(value.clone()) else {
            continue;
        };
        if event.verify().is_err() {
            continue;
        }
        let Ok(body) = validate_and_decrypt(&event, &agent, owner, rest.keys.secret_key(), owner)
        else {
            continue;
        };
        valid.push((event, body));
    }
    let Some(head_event) = select_head(valid.iter().map(|(event, _)| event.clone())) else {
        return Ok(None);
    };
    let body = valid
        .into_iter()
        .find(|(event, _)| event.id == head_event.id)
        .map(|(_, body)| body)
        .ok_or_else(|| anyhow!("runtime fence head disappeared during selection"))?;
    let Body::Memory {
        slug,
        value: Some(value),
    } = body
    else {
        bail!("runtime fence head is not a live memory value");
    };
    if slug != RUNTIME_FENCE_SLUG {
        bail!("runtime fence head has the wrong memory slug");
    }
    let lease: RuntimeFenceLease =
        serde_json::from_str(&value).context("runtime fence lease is invalid")?;
    if lease.schema != 1 || Uuid::parse_str(&lease.runtime_id).is_err() {
        bail!("runtime fence lease has an unsupported shape");
    }
    Ok(Some(RuntimeFenceHead {
        event: head_event,
        lease,
    }))
}

async fn publish_lease(
    rest: &RestClient,
    owner: &PublicKey,
    prior: Option<&RuntimeFenceHead>,
    lease: &RuntimeFenceLease,
    expected: &ExpectedFenceHead,
) -> Result<Option<RuntimeFenceHead>> {
    let value = serde_json::to_string(lease).context("failed to serialize runtime fence lease")?;
    let body = Body::Memory {
        slug: RUNTIME_FENCE_SLUG.to_string(),
        value: Some(value),
    };
    let created_at = engram::monotonic_created_at(
        now_secs(),
        prior.map(|head| head.event.created_at.as_secs()),
    );
    let expected_revision = match expected {
        ExpectedFenceHead::Missing => "missing",
        ExpectedFenceHead::Event(revision) => revision,
    };
    let event = engram::build_event_with_expected_revision(
        &rest.keys,
        owner,
        &body,
        created_at,
        Some(expected_revision),
    )
    .context("failed to build runtime fence event")?;
    let candidate_id = event.id;

    // A write can return an atomic revision conflict, or its response can be
    // lost after the relay accepted it. Exact authoritative readback settles
    // both cases without treating a transport response as lease ownership.
    let submission = rest.submit_event(&event).await;
    let readback = fetch_head(rest, owner).await?;
    if matches!(
        readback.as_ref(),
        Some(head) if head.event.id == candidate_id && head.lease == *lease
    ) {
        return Ok(readback);
    }
    if let Err(error) = submission {
        tracing::debug!(%error, "runtime fence write was not authoritative");
    }
    Ok(None)
}

pub(crate) async fn acquire(
    rest: &RestClient,
    owner: PublicKey,
    revocation: RuntimeRevocation,
) -> Result<RuntimeFenceGuard> {
    let runtime_id = Uuid::new_v4().to_string();
    for _ in 0..RUNTIME_FENCE_CAS_ATTEMPTS {
        let current = fetch_head(rest, &owner).await?;
        let revision = current.as_ref().map(|head| head.event.id.to_hex());
        match choose_acquisition(
            current
                .as_ref()
                .zip(revision.as_deref())
                .map(|(head, revision)| (&head.lease, revision)),
            &runtime_id,
            now_secs(),
        ) {
            AcquisitionDecision::HeldByOther {
                runtime_id: holder,
                lease_expires_at,
            } => {
                bail!(
                    "runtime identity is already active under lease {holder} until {lease_expires_at}"
                );
            }
            AcquisitionDecision::Write { lease, expected } => {
                if let Some(head) =
                    publish_lease(rest, &owner, current.as_ref(), &lease, &expected).await?
                {
                    tracing::info!(runtime_id, "acquired durable runtime identity fence");
                    let fail_closed_at = renewal_deadline(
                        head.lease.lease_expires_at,
                        now_secs(),
                        tokio::time::Instant::now(),
                    )
                    .ok_or_else(|| {
                        anyhow!("acquired runtime identity fence has no safe renewal window")
                    })?;
                    return Ok(RuntimeFenceGuard {
                        runtime_id,
                        owner,
                        lease_expires_at: head.lease.lease_expires_at,
                        fail_closed_at,
                        revocation,
                    });
                }
            }
        }
    }

    let current = fetch_head(rest, &owner).await?;
    if let Some(head) = current {
        if head.lease.runtime_id != runtime_id && head.lease.lease_expires_at > now_secs() {
            bail!(
                "runtime identity is already active under lease {} until {}",
                head.lease.runtime_id,
                head.lease.lease_expires_at
            );
        }
    }
    bail!("could not acquire the durable runtime identity fence after bounded CAS retries")
}

/// Why one renewal attempt did not extend the lease.
#[derive(Debug)]
enum RenewError {
    /// The durable head no longer names this runtime, or another runtime
    /// replaced it. Exclusivity is gone; the caller must revoke immediately.
    Lost(String),
    /// The relay could not be reached or did not answer. The lease itself is
    /// still valid until it expires, so the caller may retry before the safe
    /// shutdown deadline.
    Transient(anyhow::Error),
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenewError::Lost(reason) => write!(f, "{reason}"),
            RenewError::Transient(error) => write!(f, "{error}"),
        }
    }
}

async fn renew_once(rest: &RestClient, guard: &RuntimeFenceGuard) -> Result<u64, RenewError> {
    let current = fetch_head(rest, &guard.owner)
        .await
        .map_err(RenewError::Transient)?;
    let revision = current.as_ref().map(|head| head.event.id.to_hex());
    let RenewalDecision::Write {
        lease,
        expected_revision,
    } = choose_renewal(
        current
            .as_ref()
            .zip(revision.as_deref())
            .map(|(head, revision)| (&head.lease, revision)),
        &guard.runtime_id,
        now_secs(),
    )
    else {
        return Err(RenewError::Lost(
            "runtime identity fence was lost or expired".to_string(),
        ));
    };
    let expected = ExpectedFenceHead::Event(expected_revision);
    if publish_lease(rest, &guard.owner, current.as_ref(), &lease, &expected)
        .await
        .map_err(RenewError::Transient)?
        .is_none()
    {
        return Err(RenewError::Lost(
            "runtime identity fence renewal lost its atomic revision".to_string(),
        ));
    }
    Ok(lease.lease_expires_at)
}

/// How long to wait before the next renewal retry, or `None` when the safe
/// shutdown deadline has already passed and the runtime must fail closed.
fn renewal_retry_delay(
    fail_closed_at: tokio::time::Instant,
    now: tokio::time::Instant,
) -> Option<Duration> {
    let remaining = fail_closed_at.checked_duration_since(now)?;
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.min(RUNTIME_FENCE_RENEW_RETRY_DELAY))
}

pub(crate) async fn maintain(
    rest: RestClient,
    mut guard: RuntimeFenceGuard,
    loss_tx: mpsc::Sender<String>,
) {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + RUNTIME_FENCE_RENEW_INTERVAL,
        RUNTIME_FENCE_RENEW_INTERVAL,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let deadline = guard.fail_closed_at;
        if deadline <= tokio::time::Instant::now() {
            report_loss(
                &guard,
                &loss_tx,
                "runtime identity fence reached its safe shutdown deadline".to_string(),
            );
            return;
        }
        // Retry transient relay failures until the safe shutdown deadline.
        // The lease is still exclusively ours until it expires, so a blip
        // that clears within the window must not kill a healthy runtime.
        loop {
            let now = tokio::time::Instant::now();
            match tokio::time::timeout_at(deadline, renew_once(&rest, &guard)).await {
                Ok(Ok(lease_expires_at)) => {
                    let Some(fail_closed_at) =
                        renewal_deadline(lease_expires_at, now_secs(), tokio::time::Instant::now())
                    else {
                        report_loss(
                            &guard,
                            &loss_tx,
                            "renewed runtime identity fence has no safe shutdown margin"
                                .to_string(),
                        );
                        return;
                    };
                    guard.lease_expires_at = lease_expires_at;
                    guard.fail_closed_at = fail_closed_at;
                    break;
                }
                Ok(Err(RenewError::Lost(reason))) => {
                    report_loss(&guard, &loss_tx, reason);
                    return;
                }
                Ok(Err(RenewError::Transient(error))) => {
                    let Some(delay) = renewal_retry_delay(deadline, tokio::time::Instant::now())
                    else {
                        report_loss(
                            &guard,
                            &loss_tx,
                            format!(
                                "runtime identity fence renewal kept failing until its safe shutdown deadline: {error}"
                            ),
                        );
                        return;
                    };
                    tracing::warn!(
                        %error,
                        retry_in_secs = delay.as_secs(),
                        remaining_secs = deadline.saturating_duration_since(now).as_secs(),
                        "runtime identity fence renewal failed transiently — retrying before lease expiry"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(_) => {
                    report_loss(
                        &guard,
                        &loss_tx,
                        "runtime identity fence renewal exceeded its pre-expiry deadline"
                            .to_string(),
                    );
                    return;
                }
            }
        }
    }
}

fn report_loss(guard: &RuntimeFenceGuard, loss_tx: &mpsc::Sender<String>, reason: String) {
    // Revocation is the correctness boundary. Notification is only for the
    // main loop's later cleanup and must never delay worker termination.
    guard.revocation.revoke();
    let _ = loss_tx.try_send(reason);
}

/// Best-effort clean release. Crashes need no cleanup: the short lease expires.
pub(crate) async fn release(rest: &RestClient, guard: &RuntimeFenceGuard) -> Result<()> {
    let current = fetch_head(rest, &guard.owner).await?;
    let Some(current) = current else {
        return Ok(());
    };
    if current.lease.runtime_id != guard.runtime_id {
        return Ok(());
    }
    let lease = RuntimeFenceLease {
        schema: 1,
        runtime_id: guard.runtime_id.clone(),
        acquired_at: current.lease.acquired_at,
        lease_expires_at: now_secs(),
    };
    let expected = ExpectedFenceHead::Event(current.event.id.to_hex());
    let _ = publish_lease(rest, &guard.owner, Some(&current), &lease, &expected).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::AcpClient;

    fn apply_cas(
        head: &mut Option<(String, RuntimeFenceLease)>,
        revision: &str,
        expected: &ExpectedFenceHead,
        lease: RuntimeFenceLease,
    ) -> bool {
        let expectation_matches = match (expected, head.as_ref()) {
            (ExpectedFenceHead::Missing, None) => true,
            (ExpectedFenceHead::Event(expected), Some((actual, _))) => expected == actual,
            _ => false,
        };
        if expectation_matches {
            *head = Some((revision.to_string(), lease));
        }
        expectation_matches
    }

    #[test]
    fn two_runtimes_share_one_identity_but_only_the_lease_holder_may_answer() {
        let now = 1_000;
        let runtime_a = Uuid::new_v4().to_string();
        let runtime_b = Uuid::new_v4().to_string();

        // Both contenders read the same empty identity-fence head before
        // either write lands.
        let AcquisitionDecision::Write {
            lease: lease_a,
            expected: expected_a,
        } = choose_acquisition(None, &runtime_a, now)
        else {
            panic!("first contender should acquire an empty identity fence");
        };
        let AcquisitionDecision::Write {
            lease: lease_b_candidate,
            expected: expected_b,
        } = choose_acquisition(None, &runtime_b, now)
        else {
            panic!("second contender should also observe the empty identity fence");
        };

        // Relay-enforced expected-revision CAS admits exactly one candidate.
        let mut head = None;
        assert!(apply_cas(
            &mut head,
            "revision-a",
            &expected_a,
            lease_a.clone()
        ));
        assert!(!apply_cas(
            &mut head,
            "revision-b",
            &expected_b,
            lease_b_candidate
        ));
        let (head_revision, head_lease) = head.as_ref().expect("winning identity-fence head");
        assert!(runtime_is_authorized(Some(head_lease), &runtime_a, now));
        assert!(!runtime_is_authorized(Some(head_lease), &runtime_b, now));

        assert!(matches!(
            choose_acquisition(Some((head_lease, head_revision)), &runtime_b, now),
            AcquisitionDecision::HeldByOther { runtime_id, .. } if runtime_id == runtime_a
        ));
        assert!(matches!(
            choose_renewal(Some((&lease_a, "revision-a")), &runtime_b, now),
            RenewalDecision::Lost
        ));

        let after_expiry = head_lease.lease_expires_at;
        let AcquisitionDecision::Write {
            lease: lease_b,
            expected: ExpectedFenceHead::Event(revision),
        } = choose_acquisition(Some((&lease_a, "revision-a")), &runtime_b, after_expiry)
        else {
            panic!("second contender should replace an expired identity fence");
        };
        assert_eq!(revision, "revision-a");
        assert!(!runtime_is_authorized(
            Some(&lease_b),
            &runtime_a,
            after_expiry
        ));
        assert!(runtime_is_authorized(
            Some(&lease_b),
            &runtime_b,
            after_expiry
        ));
        assert!(matches!(
            choose_renewal(Some((&lease_b, "revision-b")), &runtime_a, after_expiry),
            RenewalDecision::Lost
        ));
    }

    #[test]
    fn runtime_fence_namespace_is_per_agent_identity() {
        let owner = Keys::generate().public_key();
        let first = Keys::generate();
        let second = Keys::generate();
        assert_ne!(
            runtime_fence_d_tag(&first, &owner),
            runtime_fence_d_tag(&second, &owner)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn renewal_deadline_preserves_a_shutdown_margin_before_takeover() {
        let monotonic_now = tokio::time::Instant::now();
        let lease_expires_at = 1_090;
        assert_eq!(
            renewal_deadline(lease_expires_at, 1_000, monotonic_now),
            Some(monotonic_now + Duration::from_secs(60))
        );
        assert_eq!(
            renewal_deadline(lease_expires_at, 1_060, monotonic_now),
            None,
            "renewal must fail closed 30 seconds before the durable lease expires"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocked_main_loop_cannot_delay_revocation_past_replacement_takeover() {
        let marker = std::env::temp_dir().join(format!(
            "buzz-fence-blocked-main-loop-publish-{}",
            Uuid::new_v4()
        ));
        let script = format!(
            "sleep 0.25; printf published > {}; sleep 30",
            marker.display()
        );
        let mut old_worker = AcpClient::spawn("bash", &["-c".into(), script], &[], false)
            .await
            .expect("spawn old runtime worker");

        let runtime_a = Uuid::new_v4().to_string();
        let runtime_b = Uuid::new_v4().to_string();
        let revocation = RuntimeRevocation::new();
        old_worker.bind_runtime_revocation(revocation.clone());
        let guard = RuntimeFenceGuard {
            runtime_id: runtime_a.clone(),
            owner: Keys::generate().public_key(),
            lease_expires_at: 1_090,
            fail_closed_at: tokio::time::Instant::now(),
            revocation,
        };

        // The worker is checked out by an in-flight prompt. Its AcpClient is
        // unavailable to the main event loop, which is independently blocked
        // beyond the shutdown margin and does not receive the loss notice.
        let in_flight_prompt = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(old_worker);
        });
        let (loss_tx, mut loss_rx) = mpsc::channel(1);
        let blocked_main_loop = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            loss_rx.recv().await
        });

        report_loss(&guard, &loss_tx, "test fence loss".to_string());

        // At A's durable expiry, the CAS state allows B to take over even
        // though A's main loop still has not observed the notification.
        let lease_a = RuntimeFenceLease {
            schema: 1,
            runtime_id: runtime_a,
            acquired_at: 1_000,
            lease_expires_at: 1_090,
        };
        let AcquisitionDecision::Write { lease: lease_b, .. } = choose_acquisition(
            Some((&lease_a, "runtime-a-revision")),
            &runtime_b,
            lease_a.lease_expires_at,
        ) else {
            panic!("replacement runtime should acquire at old lease expiry");
        };
        assert_eq!(lease_b.runtime_id, runtime_b);

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "old in-flight worker published after independent fence revocation"
        );
        assert!(
            !blocked_main_loop.is_finished(),
            "test main loop did not remain blocked through takeover"
        );

        in_flight_prompt.abort();
        blocked_main_loop.abort();
        let _ = in_flight_prompt.await;
        let _ = blocked_main_loop.await;
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn transient_renewal_failures_retry_until_the_safe_shutdown_deadline() {
        let now = tokio::time::Instant::now();
        let deadline = now + Duration::from_secs(27);
        assert_eq!(
            renewal_retry_delay(deadline, now),
            Some(RUNTIME_FENCE_RENEW_RETRY_DELAY),
            "a long remaining window retries at the standard delay"
        );
        assert_eq!(
            renewal_retry_delay(deadline, now + Duration::from_secs(25)),
            Some(Duration::from_secs(2)),
            "the last retry is clamped to the remaining window"
        );
        assert_eq!(
            renewal_retry_delay(deadline, deadline),
            None,
            "at the deadline the runtime fails closed instead of retrying"
        );
        assert_eq!(
            renewal_retry_delay(deadline, deadline + Duration::from_secs(1)),
            None,
            "past the deadline the runtime fails closed"
        );
    }
}
