//! Runtime-spawned ephemeral sub-agents that inherit their parent
//! agent's identity by default: same UUID, same `SecurityPolicy`, same
//! memory allowlist. A SubAgent run is auditable as a child of the
//! parent and stays inside the parent's permissions envelope.
//!
//! Two spawn sites converge on [`SubAgentSpawn`]: the agent-loop tool
//! `spawn_subagent` and the cron scheduler's `JobType::Agent` dispatch.
//! Sharing the surface keeps permission inheritance, tracing-span
//! shape, and audit attribution uniform.
//!
//! Power-users may narrow a SubAgent's permissions via
//! [`SubAgentOverrides`]; [`SubAgentSpawn::build`] validates each
//! override as a subset of the parent (using
//! [`SecurityPolicy::ensure_no_escalation_beyond`] for the policy and
//! a UUID-set containment check for the memory allowlist) and returns
//! `Err` with the originating violation chained on any escalation.

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
/// The default-everything-inherits model means the common case is
/// `SubAgentOverrides::default()` — a no-op.
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

/// Constructed SubAgent context: bound parent identity, validated
/// child policy, and the resolved memory allowlist.
#[derive(Debug, Clone)]
pub struct SubAgentContext {
    /// The parent agent's identifier. SubAgents share the parent's
    /// identity at the data layer (no separate row in the agents
    /// table); the distinction between parent and sub-run is captured
    /// at the tracing-span level (`agent.<alias>.subagent.<run_id>`).
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

/// Builder for a SubAgent spawn. The caller resolves a parent agent
/// from the loaded config; [`Self::build`] applies any narrowing
/// overrides and validates the result.
#[derive(Debug)]
pub struct SubAgentSpawn {
    pub parent_agent_id: String,
    pub parent_policy: Arc<SecurityPolicy>,
    pub parent_allowed_agent_ids: HashSet<String>,
}

impl SubAgentSpawn {
    /// Resolve a parent's identity from the loaded config and an
    /// agent alias. Returns `Err` when the alias does not name a
    /// configured agent — the spawn site surfaces a structured
    /// failure instead of invoking the agent loop on a nonexistent
    /// identity.
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
    use std::path::PathBuf;
    use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};

    fn config_with_agent(alias: &str) -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            alias.to_string(),
            AliasedAgentConfig {
                risk_profile: "default".to_string(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    #[test]
    fn for_agent_resolves_parent_identity_from_config() {
        let config = config_with_agent("alpha");
        let ctx = SubAgentSpawn::for_agent(&config, "alpha")
            .expect("for_agent must succeed for a configured agent")
            .build(SubAgentOverrides::default())
            .expect("inherits-verbatim build must succeed");
        assert_eq!(ctx.agent_id, "alpha");
        assert!(
            ctx.allowed_agent_ids.contains("alpha"),
            "an agent always sees its own rows"
        );
    }

    #[test]
    fn for_agent_errors_on_unknown_alias() {
        let err = SubAgentSpawn::for_agent(&Config::default(), "missing")
            .expect_err("unknown alias must error");
        assert!(
            err.to_string().contains("missing"),
            "expected the missing alias in the error, got: {err}"
        );
    }

    #[test]
    fn build_inherits_verbatim_when_overrides_are_default() {
        let config = config_with_agent("alpha");
        let spawn = SubAgentSpawn::for_agent(&config, "alpha").unwrap();
        let parent_policy = spawn.parent_policy.clone();
        let parent_allowlist = spawn.parent_allowed_agent_ids.clone();

        let ctx = spawn.build(SubAgentOverrides::default()).unwrap();
        assert!(Arc::ptr_eq(&ctx.policy, &parent_policy));
        assert_eq!(ctx.allowed_agent_ids, parent_allowlist);
    }

    #[test]
    fn build_rejects_policy_override_that_escalates_paths() {
        let config = config_with_agent("alpha");
        let spawn = SubAgentSpawn::for_agent(&config, "alpha").unwrap();

        let mut child_policy = (*spawn.parent_policy).clone();
        // Add an rw root the parent doesn't have — escalation.
        child_policy.allowed_roots.push(PathBuf::from("/secrets"));

        let err = spawn
            .build(SubAgentOverrides {
                policy: Some(child_policy),
                ..SubAgentOverrides::default()
            })
            .expect_err("escalating override must be rejected");
        assert!(
            err.to_string().contains("/secrets"),
            "expected the escalating path in the error chain, got: {err}"
        );
    }

    #[test]
    fn build_rejects_allowlist_override_with_id_not_on_parent() {
        let config = config_with_agent("alpha");
        let spawn = SubAgentSpawn::for_agent(&config, "alpha").unwrap();

        let mut rogue = HashSet::new();
        rogue.insert("rogue-agent".to_string());

        let err = spawn
            .build(SubAgentOverrides {
                allowed_agent_ids: Some(rogue),
                ..SubAgentOverrides::default()
            })
            .expect_err("allowlist override with foreign UUID must be rejected");
        assert!(
            err.to_string().contains("rogue-agent"),
            "expected the rogue UUID in the error chain, got: {err}"
        );
    }

    #[test]
    fn build_accepts_narrowed_allowlist_subset() {
        let config = config_with_agent("alpha");
        let spawn = SubAgentSpawn::for_agent(&config, "alpha").unwrap();

        // Empty subset is still allowed; the bound agent_id is added back.
        let ctx = spawn
            .build(SubAgentOverrides {
                allowed_agent_ids: Some(HashSet::new()),
                ..SubAgentOverrides::default()
            })
            .expect("narrowing to {} is a valid subset");
        assert_eq!(ctx.allowed_agent_ids.len(), 1);
        assert!(ctx.allowed_agent_ids.contains("alpha"));
    }
}
