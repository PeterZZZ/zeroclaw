//! SubAgent runtime (#6272 P10).
//!
//! A SubAgent is a runtime-spawned ephemeral sub-agent that inherits
//! its parent agent's identity by default. The spawning agent's UUID,
//! [`SecurityPolicy`], and memory-allowlist set carry through to the
//! SubAgent so a SubAgent run is auditable as a child of the parent
//! and stays inside the parent's permissions envelope.
//!
//! Two spawn sites in v0.8.0 converge on the [`SubAgentSpawn`] builder:
//!
//! - The agent-loop tool `spawn_subagent`, which lets a parent agent
//!   delegate a focused task at runtime.
//! - The cron scheduler's `JobType::Agent` dispatch, which runs the
//!   configured prompt under the owning agent's identity at the
//!   cron-fire moment. Both share the SubAgent infrastructure so
//!   permission inheritance, tracing span shape, and audit
//!   attribution stay uniform.
//!
//! This module ships the type-level surface and the inheritance
//! validator. The runtime-side wiring that turns a [`SubAgentContext`]
//! into a running agent loop lives in the agent module and the cron
//! scheduler dispatch path; both call [`SubAgentSpawn::build`] to
//! produce a validated context they hand to the loop builder.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::Config;

/// Optional narrowing applied to a SubAgent at spawn time. `None` on
/// every field means "inherit parent verbatim"; `Some(...)` narrows.
/// Each field is independently validated by [`SubAgentSpawn::build`]
/// to reject any value that escalates beyond the parent.
///
/// Power-users supply overrides when they want a SubAgent that has
/// narrower permissions than the parent (e.g. a research SubAgent
/// that should not have write access even though the parent does).
/// The default-everything-inherits model means the common case is
/// `SubAgentOverrides::default()`, which is a no-op.
#[derive(Debug, Clone, Default)]
pub struct SubAgentOverrides {
    /// Override the SubAgent's [`SecurityPolicy`]. Validated as a
    /// subset of the parent via
    /// [`SecurityPolicy::ensure_no_escalation_beyond`].
    pub policy: Option<SecurityPolicy>,
    /// Override the SubAgent's memory allowlist (the set of sibling
    /// agent UUIDs the SubAgent may recall from). Validated as a
    /// subset of the parent's allowlist; any UUID present here that
    /// is not on the parent's list is rejected.
    pub allowed_agent_ids: Option<HashSet<String>>,
}

/// A constructed SubAgent context: bound parent identity, validated
/// child policy, and the resolved memory allowlist. Held by the
/// runtime when a SubAgent is in flight.
#[derive(Debug, Clone)]
pub struct SubAgentContext {
    /// The parent agent's UUID. SubAgents share the parent's identity
    /// at the data layer (no separate row in the agents table); the
    /// distinction between parent and sub-run is captured at the
    /// tracing span level (`agent.<alias>.subagent.<run_id>`, P12).
    pub agent_id: String,
    /// The validated [`SecurityPolicy`] this SubAgent operates under.
    /// Identical to the parent's when `SubAgentOverrides::policy` is
    /// `None`; otherwise a narrowed copy that passed
    /// [`SecurityPolicy::ensure_no_escalation_beyond`].
    pub policy: Arc<SecurityPolicy>,
    /// Resolved memory allowlist. The bound `agent_id` is always
    /// included so the SubAgent always sees the parent's own rows;
    /// the rest is either the parent's allowlist verbatim or a
    /// validated subset.
    pub allowed_agent_ids: HashSet<String>,
}

/// Builder for a SubAgent spawn. The caller provides the parent's
/// identity, policy, and allowlist; [`Self::build`] applies any
/// narrowing overrides and validates the result.
///
/// Construction failures (escalation rejected) return
/// [`anyhow::Error`] with the specific violation chained, so the
/// caller can surface a precise message to the user instead of a
/// generic "spawn failed."
#[derive(Debug)]
pub struct SubAgentSpawn {
    pub parent_agent_id: String,
    pub parent_policy: Arc<SecurityPolicy>,
    pub parent_allowed_agent_ids: HashSet<String>,
}

impl SubAgentSpawn {
    /// Resolve a parent's identity from the loaded config and an agent
    /// alias, producing a [`SubAgentSpawn`] whose
    /// `parent_allowed_agent_ids` is derived from the agent's
    /// `[agents.<alias>.workspace.read_memory_from]` allowlist.
    ///
    /// The returned spawn is the shared starting point for both the
    /// cron `JobType::Agent` dispatch and the agent-loop
    /// `spawn_subagent` tool: each computes its own
    /// [`SubAgentOverrides`] (often `default()` = inherit verbatim) and
    /// hands it to [`Self::build`].
    ///
    /// `parent_agent_id` is the alias string in v0.8.0 because the
    /// agents-table UUID lookup is part of the P4+P9 cliff; once the
    /// cliff lands, this function will resolve to the persisted UUID
    /// instead. The spawn validator's contract is unchanged either way.
    pub fn for_agent(config: &Config, agent_alias: &str) -> Result<Self> {
        let agent = config
            .agents
            .get(agent_alias)
            .with_context(|| format!("no agent configured under alias {agent_alias:?}"))?;

        let parent_policy = SecurityPolicy::for_agent(config, agent_alias)
            .map(Arc::new)
            .with_context(|| {
                format!("could not resolve security policy for agent {agent_alias:?}")
            })?;

        let mut parent_allowed_agent_ids: HashSet<String> = agent
            .workspace
            .read_memory_from
            .iter()
            .map(|alias| alias.as_str().to_string())
            .collect();
        parent_allowed_agent_ids.insert(agent_alias.to_string());

        Ok(Self {
            parent_agent_id: agent_alias.to_string(),
            parent_policy,
            parent_allowed_agent_ids,
        })
    }

    /// Apply `overrides` to the parent's permissions and return a
    /// validated [`SubAgentContext`]. On any escalation, returns
    /// `Err` with the originating violation in the error chain.
    pub fn build(self, overrides: SubAgentOverrides) -> Result<SubAgentContext> {
        // Policy: any override must be a subset of parent.
        let policy = if let Some(child_policy) = overrides.policy {
            child_policy
                .ensure_no_escalation_beyond(&self.parent_policy)
                .map_err(|violation| {
                    anyhow::anyhow!("subagent policy override escalates beyond parent: {violation}")
                })?;
            Arc::new(child_policy)
        } else {
            self.parent_policy.clone()
        };

        // Allowlist: any override must contain only UUIDs that are
        // already on the parent's allowlist. The parent's bound
        // agent_id is always implicitly allowed (a SubAgent always
        // sees its own = the parent's rows); we add it back below.
        let allowed_agent_ids = if let Some(child_allowed) = overrides.allowed_agent_ids {
            for id in &child_allowed {
                if !self.parent_allowed_agent_ids.contains(id) {
                    anyhow::bail!(
                        "subagent allowlist override contains agent_id {id:?} not present on \
                         parent's memory allowlist; SubAgent overrides may only narrow"
                    );
                }
            }
            let mut resolved = child_allowed;
            resolved.insert(self.parent_agent_id.clone());
            resolved
        } else {
            self.parent_allowed_agent_ids
        };

        Ok(SubAgentContext {
            agent_id: self.parent_agent_id,
            policy,
            allowed_agent_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent_policy() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: "/workspace".into(),
            workspace_only: true,
            allowed_roots: vec!["/projects".into(), "/data".into()],
            allowed_roots_read_only: vec!["/shared-docs".into()],
            allowed_commands: vec!["git".into(), "cargo".into()],
            max_actions_per_hour: 100,
            max_cost_per_day_cents: 500,
            ..SecurityPolicy::default()
        })
    }

    fn parent_allowed() -> HashSet<String> {
        let mut s = HashSet::new();
        s.insert("agent-uuid-alpha".into());
        s.insert("agent-uuid-beta".into());
        s
    }

    fn parent_spawn() -> SubAgentSpawn {
        SubAgentSpawn {
            parent_agent_id: "agent-uuid-alpha".into(),
            parent_policy: parent_policy(),
            parent_allowed_agent_ids: parent_allowed(),
        }
    }

    #[test]
    fn default_overrides_inherit_parent_verbatim() {
        let ctx = parent_spawn()
            .build(SubAgentOverrides::default())
            .expect("inherit-by-default must succeed");
        assert_eq!(ctx.agent_id, "agent-uuid-alpha");
        assert_eq!(ctx.allowed_agent_ids, parent_allowed());
        assert!(
            Arc::ptr_eq(&ctx.policy, &parent_policy())
                || *ctx.policy.allowed_roots == parent_policy().allowed_roots
        );
    }

    #[test]
    fn policy_override_that_is_subset_is_accepted_and_narrows() {
        let mut narrowed = (*parent_policy()).clone();
        narrowed.allowed_roots = vec!["/projects".into()];
        narrowed.allowed_commands = vec!["git".into()];

        let ctx = parent_spawn()
            .build(SubAgentOverrides {
                policy: Some(narrowed),
                allowed_agent_ids: None,
            })
            .expect("narrowed policy must be accepted");

        assert_eq!(
            ctx.policy.allowed_roots,
            vec![std::path::PathBuf::from("/projects")]
        );
        assert_eq!(ctx.policy.allowed_commands, vec!["git".to_string()]);
    }

    #[test]
    fn policy_override_that_escalates_is_rejected_with_violation_chained() {
        let mut escalated = (*parent_policy()).clone();
        escalated.allowed_roots.push("/secrets".into());

        let err = parent_spawn()
            .build(SubAgentOverrides {
                policy: Some(escalated),
                allowed_agent_ids: None,
            })
            .expect_err("escalation must be rejected");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("escalates beyond parent"),
            "expected escalation message in chain, got: {chain}"
        );
        assert!(
            chain.contains("/secrets"),
            "expected the offending path in the chain, got: {chain}"
        );
    }

    #[test]
    fn allowlist_override_subset_is_accepted_and_always_includes_self() {
        let mut narrowed = HashSet::new();
        narrowed.insert("agent-uuid-beta".into());

        let ctx = parent_spawn()
            .build(SubAgentOverrides {
                policy: None,
                allowed_agent_ids: Some(narrowed),
            })
            .expect("narrowed allowlist must be accepted");

        // Parent's bound agent_id is implicitly added back even when
        // omitted from the override, so a SubAgent always sees its
        // own (= parent's) memory rows.
        assert!(ctx.allowed_agent_ids.contains("agent-uuid-alpha"));
        assert!(ctx.allowed_agent_ids.contains("agent-uuid-beta"));
        assert_eq!(ctx.allowed_agent_ids.len(), 2);
    }

    #[test]
    fn for_agent_resolves_parent_identity_from_config() {
        use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

        let mut config = Config {
            workspace_dir: std::path::PathBuf::from("/tmp/zeroclaw-test"),
            ..Config::default()
        };
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            "alpha".to_string(),
            AliasedAgentConfig {
                risk_profile: "default".to_string(),
                ..AliasedAgentConfig::default()
            },
        );
        let mut beta = AliasedAgentConfig {
            risk_profile: "default".to_string(),
            ..AliasedAgentConfig::default()
        };
        beta.workspace.read_memory_from = vec!["alpha".into()];
        config.agents.insert("beta".to_string(), beta);

        let spawn = SubAgentSpawn::for_agent(&config, "beta")
            .expect("for_agent must succeed for a configured agent");
        assert_eq!(spawn.parent_agent_id, "beta");
        // Bound agent_id always re-included; the read_memory_from
        // entry is also present.
        assert!(spawn.parent_allowed_agent_ids.contains("beta"));
        assert!(spawn.parent_allowed_agent_ids.contains("alpha"));
        assert_eq!(spawn.parent_allowed_agent_ids.len(), 2);
    }

    #[test]
    fn for_agent_errors_on_unknown_alias() {
        use zeroclaw_config::schema::Config;

        let config = Config::default();
        let err =
            SubAgentSpawn::for_agent(&config, "missing").expect_err("unknown alias must error");
        assert!(
            err.to_string().contains("missing"),
            "expected the missing alias in the error, got: {err}"
        );
    }

    #[test]
    fn allowlist_override_with_rogue_uuid_is_rejected() {
        let mut rogue = HashSet::new();
        rogue.insert("agent-uuid-rogue".into());

        let err = parent_spawn()
            .build(SubAgentOverrides {
                policy: None,
                allowed_agent_ids: Some(rogue),
            })
            .expect_err("rogue UUID must be rejected");
        assert!(
            err.to_string().contains("agent-uuid-rogue"),
            "expected rogue UUID in error, got: {err}"
        );
    }
}
