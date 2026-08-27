//! Distribution policy at managed-agent enforcement boundaries.
//!
//! ## What this build capability guarantees, and what it does not
//!
//! `BUZZ_BUILD_AGENT_ACCESS_OWNER_ONLY` marks a build whose managed agents may
//! answer only their owner. Enforcement is applied at the two boundaries where
//! Desktop hands access to something that runs the agent, and nowhere else. The
//! stored record and its relay-advertised access fields are left untouched, so
//! the same profile keeps its user-chosen access when it is opened in an OSS
//! build.
//!
//! Enforced:
//!
//! - **Local spawn.** [`build_respond_to_env_with_policy`] clamps
//!   `BUZZ_ACP_RESPOND_TO` to `owner-only` and pins the independent
//!   `BUZZ_ACP_ALLOWED_RESPOND_TO=owner-only` guard on every start, whatever
//!   the record says.
//! - **Provider deployment, including upgrades.**
//!   [`projected_access_with_policy`] projects owner-only into every payload.
//!   Workspace apply redeploys each existing provider agent before the marked
//!   build renders community UI. A failed redeploy fails the apply, so Desktop
//!   does not present the locked owner-only control as applied while the remote
//!   deployment may still use a wider policy.
//!
//! ## "owner-only" is owner plus verified same-owner sibling agents
//!
//! The harness gate this projection targets admits the human owner *and* every
//! cryptographically NIP-OA-verified agent that shares that owner (see
//! `crates/buzz-acp/src/lib.rs`). That is the intended boundary, not an
//! oversight: an owner's own agents are inside their trust boundary, and Buzz's
//! built-in Welcome team relies on it, because the lead instructs its teammates
//! while every teammate is created owner-only (see
//! `welcomeTeammateHasExpectedAccess` in
//! `desktop/src/features/onboarding/welcomeGuide.ts`). Read every use of
//! "owner-only" in this module as `owner ∪ verified same-owner agents`. The
//! setting's own copy says so: the line under Only me reads "Only you and your
//! agents can send instructions." (`RespondToField.tsx`). The dropdown label
//! stays "Only me", which is the audience the user picks.

use super::{validate_respond_to_allowlist, ManagedAgentRecord, RespondTo};

pub(crate) type RespondToEnv = (Vec<(&'static str, String)>, Vec<&'static str>);
pub(crate) const DRIVER_CONTINUATION_INTERVAL_SECONDS: &str = "900";

/// Internal managed agents all receive the same driver-neutral continuation
/// heartbeat. The schedule is still empty unless that agent is the designated
/// driver and explicitly records a delegation, so enabling the timer does not
/// create a universal supervisor or cross-agent work queue.
pub(crate) fn driver_follow_through_env_with_policy(
    internal_managed_build: bool,
) -> Vec<(&'static str, String)> {
    if internal_managed_build {
        vec![
            (
                "BUZZ_ACP_HEARTBEAT_INTERVAL",
                DRIVER_CONTINUATION_INTERVAL_SECONDS.to_string(),
            ),
            ("BUZZ_ACP_HEARTBEAT_MODE", "schedules".to_string()),
        ]
    } else {
        Vec::new()
    }
}

/// Apply the internal continuation policy after the fully layered local
/// descriptor environment so an internal record cannot shadow it. OSS builds
/// intentionally leave any user-supplied heartbeat configuration untouched.
pub(crate) fn enforce_driver_heartbeat(
    command: &mut std::process::Command,
    internal_managed_build: bool,
) {
    for (key, value) in driver_follow_through_env_with_policy(internal_managed_build) {
        command.env(key, value);
    }
}

/// Release packaging sets `BUZZ_BUILD_AGENT_ACCESS_OWNER_ONLY`; OSS/custom
/// builds do not.
pub(crate) fn owner_only_access_build() -> bool {
    option_env!("BUZZ_DESKTOP_BUILD_AGENT_ACCESS_OWNER_ONLY").is_some()
}

pub(crate) fn owner_only() -> bool {
    owner_only_with_policy(owner_only_access_build())
}

pub(crate) fn owner_only_with_policy(owner_only_access: bool) -> bool {
    owner_only_access
}

/// Project effective access at a behavioral boundary without changing the
/// stored or relay-advertised access fields.
pub(crate) fn projected_access_with_policy(
    record: &ManagedAgentRecord,
    owner_only_access: bool,
) -> (RespondTo, Vec<String>) {
    if owner_only_with_policy(owner_only_access) {
        (RespondTo::OwnerOnly, Vec::new())
    } else {
        (record.respond_to, record.respond_to_allowlist.clone())
    }
}

/// Build the inbound-author access environment for a launched agent. The
/// explicit policy input keeps owner-only access enforcement testable without
/// weakening the production caller's compile-time decision.
pub(crate) fn build_respond_to_env_with_policy(
    record: &ManagedAgentRecord,
    owner_hex: Option<&str>,
    enforced_owner_only: bool,
) -> Result<RespondToEnv, String> {
    let (respond_to, _) = projected_access_with_policy(record, enforced_owner_only);
    let normalized = validate_respond_to_allowlist(&record.respond_to_allowlist)?;
    if respond_to == RespondTo::Allowlist && normalized.is_empty() {
        return Err(
            "respond-to mode 'allowlist' requires at least one pubkey in the allowlist".to_string(),
        );
    }

    let mut set = vec![("BUZZ_ACP_RESPOND_TO", respond_to.as_str().to_string())];
    let mut remove = Vec::new();
    if enforced_owner_only {
        set.push((
            "BUZZ_ACP_ALLOWED_RESPOND_TO",
            RespondTo::OwnerOnly.as_str().to_string(),
        ));
    } else {
        remove.push("BUZZ_ACP_ALLOWED_RESPOND_TO");
    }
    if respond_to == RespondTo::Allowlist {
        set.push(("BUZZ_ACP_RESPOND_TO_ALLOWLIST", normalized.join(",")));
    } else {
        remove.push("BUZZ_ACP_RESPOND_TO_ALLOWLIST");
    }

    if record.auth_tag.is_none() {
        if let Some(owner) = owner_hex {
            set.push(("BUZZ_ACP_AGENT_OWNER", owner.to_string()));
        } else {
            remove.push("BUZZ_ACP_AGENT_OWNER");
        }
    } else {
        remove.push("BUZZ_ACP_AGENT_OWNER");
    }
    Ok((set, remove))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_agents::BackendKind;

    fn record(backend: BackendKind) -> ManagedAgentRecord {
        let mut record: ManagedAgentRecord = serde_json::from_value(serde_json::json!({
            "pubkey": "agent", "name": "Agent", "relay_url": "", "acp_command": "",
            "agent_command": "", "agent_args": [], "mcp_command": "",
            "turn_timeout_seconds": 0, "system_prompt": null, "created_at": "",
            "updated_at": "", "last_started_at": null, "last_stopped_at": null,
            "last_exit_code": null, "last_error": null
        }))
        .unwrap();
        record.backend = backend;
        record.respond_to = RespondTo::Anyone;
        record.respond_to_allowlist = vec!["a".repeat(64)];
        record
    }

    #[test]
    fn owner_only_access_policy_rejects_malformed_stored_allowlist_before_clamping() {
        let mut record = record(BackendKind::Local);
        record.respond_to_allowlist = vec!["malformed stale allowlist".into()];

        let error = build_respond_to_env_with_policy(&record, Some("owner"), true)
            .expect_err("owner-only access policy accepted a malformed stored allowlist");

        assert!(
            error.contains("invalid pubkey in respond-to allowlist"),
            "owner-only access policy returned the wrong malformed-allowlist error: {error}",
        );
    }

    #[test]
    fn owner_only_access_enforcement_clamps_local_and_provider() {
        for (label, backend) in [
            ("local", BackendKind::Local),
            (
                "provider",
                BackendKind::Provider {
                    id: "p".into(),
                    config: serde_json::json!({}),
                },
            ),
        ] {
            let record = record(backend);
            let (gate_set, _) =
                build_respond_to_env_with_policy(&record, Some("owner"), true).unwrap();
            let gate_set: std::collections::HashMap<_, _> = gate_set.into_iter().collect();
            assert_eq!(
                gate_set.get("BUZZ_ACP_RESPOND_TO").map(String::as_str),
                Some("owner-only"),
                "owner-only runtime env did not clamp {label} agent",
            );
            assert_eq!(
                gate_set
                    .get("BUZZ_ACP_ALLOWED_RESPOND_TO")
                    .map(String::as_str),
                Some("owner-only"),
                "owner-only runtime env omitted the {label} agent guard",
            );
            let (respond_to, allowlist) = projected_access_with_policy(&record, true);
            assert_eq!(
                respond_to,
                RespondTo::OwnerOnly,
                "owner-only provider payload did not clamp {label} agent",
            );
            assert!(
                allowlist.is_empty(),
                "owner-only provider payload retained {label} agent allowlist",
            );
        }
    }

    #[test]
    fn internal_build_enables_driver_neutral_fifteen_minute_continuation() {
        let env: std::collections::HashMap<_, _> = driver_follow_through_env_with_policy(true)
            .into_iter()
            .collect();
        assert_eq!(
            env.get("BUZZ_ACP_HEARTBEAT_INTERVAL").map(String::as_str),
            Some("900")
        );
        assert_eq!(
            env.get("BUZZ_ACP_HEARTBEAT_MODE").map(String::as_str),
            Some("schedules")
        );
        for key in env.keys() {
            assert!(!super::super::env_vars::is_reserved_env_key(key));
        }
        assert!(driver_follow_through_env_with_policy(false).is_empty());
    }

    #[test]
    fn local_internal_heartbeat_clamps_after_user_env_while_oss_passes_through() {
        let mut internal = std::process::Command::new("unused");
        internal
            .env("BUZZ_ACP_HEARTBEAT_INTERVAL", "60")
            .env("BUZZ_ACP_HEARTBEAT_MODE", "feed");
        enforce_driver_heartbeat(&mut internal, true);
        let internal_env: std::collections::BTreeMap<_, _> = internal
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(
            internal_env[std::ffi::OsStr::new("BUZZ_ACP_HEARTBEAT_INTERVAL")],
            "900"
        );
        assert_eq!(
            internal_env[std::ffi::OsStr::new("BUZZ_ACP_HEARTBEAT_MODE")],
            "schedules"
        );

        let mut oss = std::process::Command::new("unused");
        oss.env("BUZZ_ACP_HEARTBEAT_INTERVAL", "60")
            .env("BUZZ_ACP_HEARTBEAT_MODE", "feed");
        enforce_driver_heartbeat(&mut oss, false);
        let oss_env: std::collections::BTreeMap<_, _> = oss
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(
            oss_env[std::ffi::OsStr::new("BUZZ_ACP_HEARTBEAT_INTERVAL")],
            "60"
        );
        assert_eq!(
            oss_env[std::ffi::OsStr::new("BUZZ_ACP_HEARTBEAT_MODE")],
            "feed"
        );
    }
}
