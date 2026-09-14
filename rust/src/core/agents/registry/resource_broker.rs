use chrono::Utc;

use super::{AgentEntry, AgentStatus};

const HARD_MAX_CONCURRENT_WORKERS: usize = 15;

pub(super) fn max_concurrent_workers() -> usize {
    crate::core::config::Config::load()
        .agents
        .max_concurrent_workers
        .clamp(1, HARD_MAX_CONCURRENT_WORKERS)
}

pub(super) fn max_concurrent_mutating_workers() -> usize {
    crate::core::config::Config::load()
        .agents
        .max_concurrent_mutating_workers
        .clamp(1, HARD_MAX_CONCURRENT_WORKERS)
}

pub(super) fn role_can_mutate(role: Option<&str>) -> bool {
    let normalized = role.unwrap_or_default().to_ascii_lowercase();
    ![
        "analysis",
        "analyst",
        "audit",
        "mapper",
        "research",
        "review",
        "reviewer",
        "read-only",
        "readonly",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

pub(super) fn active_worker_lease_seconds() -> i64 {
    i64::try_from(
        crate::core::config::Config::load()
            .agents
            .active_worker_lease_seconds,
    )
    .unwrap_or(i64::MAX)
    .max(1)
}

pub(super) fn has_active_worker_lease(agent: &AgentEntry, now: chrono::DateTime<Utc>) -> bool {
    agent.status == AgentStatus::Active
        && now.signed_duration_since(agent.last_active).num_seconds()
            <= active_worker_lease_seconds()
}

pub(super) fn consumes_worker_capacity(agent: &AgentEntry) -> bool {
    registration_consumes_capacity(&agent.agent_type, agent.role.as_deref())
}

/// Whether a registration with this type/role takes a machine-wide lease.
/// The construction-time `context-engine` placeholder of an MCP process is
/// the one presence that never does — so it must also never be *refused* by
/// a cap it does not count against (#1765).
pub(super) fn registration_consumes_capacity(agent_type: &str, role: Option<&str>) -> bool {
    !(agent_type == "mcp" && role == Some("context-engine"))
}

/// `(takes a machine-wide lease, takes a mutating slot)` for a registration.
/// Used to decide whether a role change on an existing record escalates and
/// therefore has to pass the caps like a fresh registration (#1765).
pub(super) fn capacity_profile(agent_type: &str, role: Option<&str>) -> (bool, bool) {
    let consumes = registration_consumes_capacity(agent_type, role);
    (consumes, consumes && role_can_mutate(role))
}

/// The role a session is admitted with when the mutating cap is full (#1765).
/// It holds a machine-wide lease — peers can see it — but never a mutating
/// slot: `role_can_mutate` classifies it read-only by its marker.
pub(super) const READ_ONLY_PRESENCE_ROLE: &str = "read-only";

/// Stable prefix of the mutating-cap rejection. Callers match on it to take
/// the read-only admission path instead of losing the whole session.
pub(crate) const MUTATING_CAPACITY_MARKER: &str = "machine-wide mutating-agent capacity reached";

pub(super) fn mutating_capacity_message(active_mutating: usize, limit: usize) -> String {
    format!(
        "{MUTATING_CAPACITY_MARKER}: {active_mutating}/{limit}; a slot frees when another \
         worker finishes or its {}s lease expires. A session over the cap is admitted \
         read-only (ctx_read, ctx_search, ctx_compose, …) and retries the mutating slot \
         on its next mutating tool call",
        active_worker_lease_seconds()
    )
}

pub(super) fn ensure_worker_capacity(
    active_machine_wide: usize,
    limit: usize,
    project_root: &str,
) -> Result<(), String> {
    if active_machine_wide < limit {
        return Ok(());
    }
    Err(format!(
        "machine-wide agent capacity reached while registering {project_root}: {active_machine_wide}/{limit} active leases; finish or idle a session before starting another"
    ))
}
