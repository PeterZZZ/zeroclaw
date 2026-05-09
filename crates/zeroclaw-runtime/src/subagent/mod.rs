//! SubAgent runtime (#6272).
//!
//! A SubAgent is a runtime-spawned ephemeral sub-agent that inherits
//! its parent agent's identity verbatim. The parent's UUID,
//! `SecurityPolicy`, and memory-allowlist set carry through to the
//! SubAgent so a SubAgent run is auditable as a child of the parent
//! and stays inside the parent's permissions envelope.
//!
//! Two spawn sites in v0.8.0 use the [`SubAgentSpawn`] surface:
//!
//! - The agent-loop tool `spawn_subagent`, which lets a parent agent
//!   delegate a focused task at runtime.
//! - The cron scheduler's `JobType::Agent` dispatch, which runs the
//!   configured prompt under the owning agent's identity at the
//!   cron-fire moment. Both share the SubAgent infrastructure so
//!   permission inheritance, tracing-span shape, and audit
//!   attribution stay uniform across spawn sites.
//!
//! v0.8.0 inherits-verbatim only. The narrowing-override surface
//! (`SubAgentOverrides` + `SecurityPolicy::ensure_no_escalation_beyond`
//! validator) lands in v0.8.1 alongside the
//! `[agents.<alias>].subagent_*` config block that supplies the
//! overrides; the validator is in `crates/zeroclaw-config/src/policy.rs`
//! ready for that wiring.

use anyhow::{Context, Result};

use zeroclaw_config::schema::Config;

/// A constructed SubAgent context: the parent agent's identity, used
/// by spawn-site code to populate the `subagent` tracing span and
/// route audit log entries.
#[derive(Debug, Clone)]
pub struct SubAgentContext {
    /// The parent agent's identifier (alias in v0.8.0; agents-table
    /// UUID once the runtime cuts over to UUID-keyed identity).
    /// SubAgents share the parent's identity at the data layer (no
    /// separate row in the agents table); the distinction between
    /// parent and sub-run is captured at the tracing-span level
    /// (`agent.<alias>.subagent.<run_id>`).
    pub agent_id: String,
}

/// Builder for a SubAgent spawn. The caller resolves a parent agent
/// from the loaded config; [`Self::build`] returns the inherits-
/// verbatim [`SubAgentContext`] for that parent.
#[derive(Debug)]
pub struct SubAgentSpawn {
    pub parent_agent_id: String,
}

impl SubAgentSpawn {
    /// Resolve a parent's identity from the loaded config and an
    /// agent alias.
    ///
    /// Returns `Err` when the alias does not name a configured agent
    /// — the spawn site surfaces a structured failure instead of
    /// invoking the agent loop on a nonexistent identity.
    pub fn for_agent(config: &Config, agent_alias: &str) -> Result<Self> {
        config
            .agents
            .get(agent_alias)
            .with_context(|| format!("no agent configured under alias {agent_alias:?}"))?;
        Ok(Self {
            parent_agent_id: agent_alias.to_string(),
        })
    }

    /// Produce the inherits-verbatim [`SubAgentContext`].
    #[must_use]
    pub fn build(self) -> SubAgentContext {
        SubAgentContext {
            agent_id: self.parent_agent_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};

    #[test]
    fn for_agent_resolves_parent_identity_from_config() {
        let mut config = Config::default();
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

        let ctx = SubAgentSpawn::for_agent(&config, "alpha")
            .expect("for_agent must succeed for a configured agent")
            .build();
        assert_eq!(ctx.agent_id, "alpha");
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
}
