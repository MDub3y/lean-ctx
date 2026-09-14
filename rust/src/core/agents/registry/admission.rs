//! Presence admission (#1765 / #1766): the capacity checks a registration
//! has to pass, and the MCP-process entry points that rely on them.
//!
//! Split out of `registry.rs` so the parent stays under the LOC gate; the
//! `impl` block below is part of [`AgentRegistry`] like any other.

use chrono::Utc;

use super::resource_broker::{
    MUTATING_CAPACITY_MARKER, READ_ONLY_PRESENCE_ROLE, consumes_worker_capacity,
    ensure_worker_capacity, has_active_worker_lease, max_concurrent_mutating_workers,
    max_concurrent_workers, mutating_capacity_message, registration_consumes_capacity,
    role_can_mutate,
};
use super::{
    AgentEntry, AgentRegistry, AgentStatus, ProcessIdentityIndex, mutate_persistent, presence_ttl,
    process_identity_matches,
};

impl AgentRegistry {
    /// The machine-wide lease cap, the mutating-slot cap and the one-role-per-
    /// project rule for a registration about to take effect. `exclude_agent_id`
    /// is the record being upgraded in place, so it never counts against
    /// itself. A registration that takes no lease at all is never refused by
    /// a cap it does not count against (#1765).
    pub(super) fn ensure_registration_capacity(
        &self,
        agent_type: &str,
        role: Option<&str>,
        project_root: &str,
        exclude_agent_id: Option<&str>,
        compatibility_identities: &ProcessIdentityIndex,
        now: chrono::DateTime<Utc>,
    ) -> Result<(), String> {
        if !registration_consumes_capacity(agent_type, role) {
            return Ok(());
        }
        let is_counted = |agent: &AgentEntry| {
            exclude_agent_id.is_none_or(|own| agent.agent_id != own)
                && consumes_worker_capacity(agent)
                && has_active_worker_lease(agent, now)
                && process_identity_matches(agent, compatibility_identities)
        };

        let active_machine_wide = self.agents.iter().filter(|a| is_counted(a)).count();
        ensure_worker_capacity(active_machine_wide, max_concurrent_workers(), project_root)?;

        if role_can_mutate(role) {
            let active_mutating = self
                .agents
                .iter()
                .filter(|agent| is_counted(agent) && role_can_mutate(agent.role.as_deref()))
                .count();
            let limit = max_concurrent_mutating_workers();
            if active_mutating >= limit {
                return Err(mutating_capacity_message(active_mutating, limit));
            }
        }

        // The read-only admission is a placeholder like `context-engine`, not
        // a worker role: two sessions over the cap on one project must both
        // still be admitted.
        if let Some(role) = role.map(str::trim).filter(|role| !role.is_empty())
            && role != READ_ONLY_PRESENCE_ROLE
            && self.agents.iter().any(|agent| {
                exclude_agent_id.is_none_or(|own| agent.agent_id != own)
                    && agent.project_root == project_root
                    && agent
                        .role
                        .as_deref()
                        .is_some_and(|active| active.eq_ignore_ascii_case(role))
                    && has_active_worker_lease(agent, now)
                    && process_identity_matches(agent, compatibility_identities)
            })
        {
            return Err(format!(
                "duplicate active role rejected for {project_root}: {role}; reuse or finish the existing worker"
            ));
        }
        Ok(())
    }

    /// Atomically registers this MCP process in the shared on-disk registry.
    pub(crate) fn register_mcp_process(project_root: &str) -> Result<String, String> {
        Self::register_mcp_process_as(project_root, "context-engine")
    }

    /// Atomically registers this MCP process with the role the session
    /// resolved at `initialize` (#1766). `register_mcp_process` is the
    /// construction-time placeholder; once the real role is known, the
    /// fail-closed presence retry must use this instead.
    pub(crate) fn register_mcp_process_as(
        project_root: &str,
        role: &str,
    ) -> Result<String, String> {
        mutate_persistent(|registry| {
            registry.cleanup_stale(presence_ttl());
            registry.register("mcp", Some(role), project_root)
        })
        .and_then(|result| result)
    }

    /// Admits this MCP process read-only after the mutating cap refused its
    /// real role (#1765). The presence holds a machine-wide lease — peers can
    /// see it — but no mutating slot; the status message records the role it
    /// asked for so the registry stays honest about what is running.
    pub(crate) fn admit_read_only_presence(
        project_root: &str,
        requested_role: &str,
    ) -> Result<String, String> {
        mutate_persistent(|registry| {
            registry.cleanup_stale(presence_ttl());
            let agent_id =
                registry.register("mcp", Some(READ_ONLY_PRESENCE_ROLE), project_root)?;
            registry.set_status(
                &agent_id,
                AgentStatus::Active,
                Some(&format!(
                    "admitted read-only: {MUTATING_CAPACITY_MARKER} while registering as {requested_role}"
                )),
            )?;
            Ok(agent_id)
        })
        .and_then(|result| result)
    }

    /// True when a registration failed because the machine-wide mutating cap
    /// is full — the one failure a session recovers from by being admitted
    /// read-only (#1765).
    pub(crate) fn is_mutating_capacity_error(error: &str) -> bool {
        error.contains(MUTATING_CAPACITY_MARKER)
    }
}

/// #1765 / #1766: capacity admission and role stability of the MCP presence.
#[cfg(all(test, unix))]
mod tests {
    use chrono::Utc;

    use super::super::{AgentEntry, AgentRegistry, AgentStatus, ProcessIdentityIndex};

    /// A registry whose mutating cap is 1 and already holds one active
    /// mutating worker owned by a *different* live process (the test
    /// runner's parent), so the current process is the one over the cap.
    fn registry_with_full_mutating_cap() -> (
        AgentRegistry,
        crate::core::data_dir::IsolatedDataDir,
        tempfile::TempDir,
    ) {
        let isolated = crate::core::data_dir::isolated_data_dir();
        let config_dir = tempfile::tempdir().expect("config dir");
        std::fs::write(
            config_dir.path().join("config.toml"),
            "[agents]\nmax_concurrent_mutating_workers = 1\n",
        )
        .expect("write config");
        crate::test_env::set_var("LEAN_CTX_CONFIG_DIR", config_dir.path());

        let filler_pid = std::os::unix::process::parent_id();
        let now = Utc::now();
        let mut registry = AgentRegistry::new();
        registry.agents.push(AgentEntry {
            agent_id: "cursor-filler".to_string(),
            agent_type: "cursor".to_string(),
            role: Some("coder".to_string()),
            project_root: "/elsewhere".to_string(),
            started_at: now,
            last_active: now,
            pid: filler_pid,
            process_identity: crate::ipc::process::identity(filler_pid),
            status: AgentStatus::Active,
            status_message: None,
        });
        assert!(
            registry.agents[0].process_identity.is_some(),
            "the parent process must have a readable identity"
        );
        (registry, isolated, config_dir)
    }

    fn role_of<'a>(registry: &'a AgentRegistry, agent_id: &str) -> Option<&'a str> {
        registry
            .agents
            .iter()
            .find(|agent| agent.agent_id == agent_id)
            .and_then(|agent| agent.role.as_deref())
    }

    #[test]
    fn a_mutating_role_over_the_cap_is_refused_with_the_marker() {
        let (mut registry, _iso, _cfg) = registry_with_full_mutating_cap();
        let error = registry
            .register("mcp", Some("coder"), "/project")
            .expect_err("the mutating cap is full");
        assert!(AgentRegistry::is_mutating_capacity_error(&error), "{error}");
        assert!(error.contains("1/1"), "{error}");
        assert!(
            error.contains("admitted read-only"),
            "the message must name the recovery path: {error}"
        );
        crate::test_env::remove_var("LEAN_CTX_CONFIG_DIR");
    }

    #[test]
    fn a_non_consuming_presence_is_never_refused_by_a_full_cap() {
        // #1765: `context-engine` counts against no cap, so a full cap must
        // not refuse it — that refusal was the "every tool call fails, reads
        // included" symptom.
        let (mut registry, _iso, _cfg) = registry_with_full_mutating_cap();
        registry
            .register("mcp", Some("context-engine"), "/project")
            .expect("a placeholder presence is admitted");
        crate::test_env::remove_var("LEAN_CTX_CONFIG_DIR");
    }

    #[test]
    fn a_read_only_admission_persists_the_requested_role_in_its_status() {
        let (registry, _iso, _cfg) = registry_with_full_mutating_cap();
        registry.save().expect("persist the filler");

        let id = AgentRegistry::admit_read_only_presence("/project", "coder")
            .expect("read-only takes no mutating slot");
        let loaded = AgentRegistry::load().expect("registry");
        let me = loaded
            .agents
            .iter()
            .find(|agent| agent.agent_id == id)
            .expect("the presence is on disk");
        assert_eq!(me.role.as_deref(), Some("read-only"));
        assert!(
            me.status_message
                .as_deref()
                .unwrap_or_default()
                .contains("coder"),
            "the status must say which role was requested: {:?}",
            me.status_message
        );

        // A second over-cap session on the same project is admitted too: the
        // read-only placeholder is exempt from the one-role-per-project rule.
        loaded
            .ensure_registration_capacity(
                "mcp",
                Some("read-only"),
                "/project",
                None,
                &ProcessIdentityIndex::load(),
                Utc::now(),
            )
            .expect("read-only is never a duplicate role");

        // Once the other worker is gone, the on-disk upgrade to the requested
        // role succeeds in place and the admission note does not outlive it.
        let mut freed = loaded;
        freed
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == "cursor-filler")
            .expect("filler")
            .status = AgentStatus::Finished;
        freed.save().expect("persist the freed slot");
        let upgraded = AgentRegistry::register_mcp_process_as("/project", "coder")
            .expect("the slot is free now");
        assert_eq!(upgraded, id, "same record, not a duplicate");
        let after = AgentRegistry::load().expect("registry");
        let me = after
            .agents
            .iter()
            .find(|agent| agent.agent_id == id)
            .expect("the presence is on disk");
        assert_eq!(me.role.as_deref(), Some("coder"));
        assert_eq!(
            me.status_message, None,
            "a role change clears the role-bound admission note"
        );
        crate::test_env::remove_var("LEAN_CTX_CONFIG_DIR");
    }

    #[test]
    fn upgrading_a_placeholder_to_a_mutating_role_is_checked_against_the_cap() {
        // Guard for the fix itself: with the placeholder exempt from the caps,
        // the `initialize` upgrade to the real role is where the cap has to
        // bite — before, an in-place update was never checked at all.
        let (mut registry, _iso, _cfg) = registry_with_full_mutating_cap();
        let placeholder = registry
            .register("mcp", Some("context-engine"), "/project")
            .expect("placeholder");
        let error = registry
            .register("mcp", Some("coder"), "/project")
            .expect_err("the upgrade is over the cap");
        assert!(AgentRegistry::is_mutating_capacity_error(&error), "{error}");
        assert_eq!(
            role_of(&registry, &placeholder),
            Some("context-engine"),
            "a refused upgrade leaves the record unchanged"
        );

        // Once the other worker is gone, the same upgrade succeeds in place.
        registry.agents[0].status = AgentStatus::Finished;
        let upgraded = registry
            .register("mcp", Some("coder"), "/project")
            .expect("the slot is free now");
        assert_eq!(upgraded, placeholder, "same record, not a duplicate");
        assert_eq!(role_of(&registry, &placeholder), Some("coder"));
        crate::test_env::remove_var("LEAN_CTX_CONFIG_DIR");
    }

    #[test]
    fn the_placeholder_retry_never_overwrites_an_explicit_role() {
        // #1766: the fail-closed presence retry registers as `context-engine`
        // for the same PID; a `reviewer` (or `coder`) must keep its role and
        // its place in capacity accounting.
        let _iso = crate::core::data_dir::isolated_data_dir();
        let mut registry = AgentRegistry::new();
        let id = registry
            .register("mcp", Some("reviewer"), "/project")
            .expect("reviewer");
        let again = registry
            .register("mcp", Some("context-engine"), "/project")
            .expect("retry for the same process");
        assert_eq!(id, again);
        assert_eq!(role_of(&registry, &id), Some("reviewer"));

        // An explicit role still replaces the placeholder: the initialize
        // upgrade from the construction-time presence must keep working.
        let mut fresh = AgentRegistry::new();
        let placeholder = fresh
            .register("mcp", Some("context-engine"), "/project")
            .expect("placeholder");
        fresh
            .register("mcp", Some("debugger"), "/project")
            .expect("upgrade");
        assert_eq!(role_of(&fresh, &placeholder), Some("debugger"));
    }
}
