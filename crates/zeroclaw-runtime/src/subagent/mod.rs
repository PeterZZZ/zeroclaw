//! Runtime-spawned ephemeral sub-agents that inherit their parent
//! agent's identity verbatim: same UUID, same `SecurityPolicy`, same
//! memory allowlist. A SubAgent run is auditable as a child of the
//! parent and stays inside the parent's permissions envelope.
//!
//! Two spawn sites converge on [`SubAgentSpawn`]: the agent-loop
//! tool `spawn_subagent` and the cron scheduler's `JobType::Agent`
//! dispatch. Sharing the surface keeps permission inheritance,
//! tracing-span shape, and audit attribution uniform.

use anyhow::{Context, Result};

use zeroclaw_config::schema::Config;

/// Constructed SubAgent context: the parent agent's identity, used
/// by spawn-site code to populate the `subagent` tracing span and
/// route audit log entries.
#[derive(Debug, Clone)]
pub struct SubAgentContext {
    /// The parent agent's identifier. SubAgents share the parent's
    /// identity at the data layer (no separate row in the agents
    /// table); the distinction between parent and sub-run is captured
    /// at the tracing-span level (`agent.<alias>.subagent.<run_id>`).
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
