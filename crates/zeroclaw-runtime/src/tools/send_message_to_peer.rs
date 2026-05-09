//! Agent-loop tool that sends a message to a configured peer on a
//! shared channel.
//!
//! Validates the target against [`crate::peers::ResolvedPeers`] for
//! the calling agent on the requested channel: peers must mutually
//! opt in via a `[peer_groups.<name>]` block whose `agents` lists
//! both, OR appear on the group's `external_peers` list, before this
//! tool will deliver. Cross-channel sends from outside the resolver's
//! authorization surface are rejected.
//!
//! Delivery itself routes through [`crate::cron::scheduler::deliver_announcement`],
//! which forwards to the channel registry the binary registers at
//! startup. The tool does not import the channels crate directly to
//! preserve the runtime → channels dependency direction.

use crate::cron::scheduler::deliver_announcement;
use crate::peers::resolve_peer_set;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::providers::ChannelRef;
use zeroclaw_config::schema::Config;

/// Send a message to a peer on a shared channel. Bound to a single
/// calling agent's alias; the tool validates every send against that
/// agent's resolved peer set.
pub struct SendMessageToPeerTool {
    config: Arc<Config>,
    sender_alias: String,
}

impl SendMessageToPeerTool {
    pub fn new(config: Arc<Config>, sender_alias: impl Into<String>) -> Self {
        Self {
            config,
            sender_alias: sender_alias.into(),
        }
    }
}

#[async_trait]
impl Tool for SendMessageToPeerTool {
    fn name(&self) -> &str {
        "send_message_to_peer"
    }

    fn description(&self) -> &str {
        "Send a message to a peer agent or external peer (human, external bot) \
         on a shared channel. The target must be a member of a peer group both \
         this agent and the target agree on (or an external peer listed on the \
         shared group's `external_peers`). Cross-agent sends to non-peers are \
         rejected at the tool boundary; the channel send only happens after \
         the peer-set check passes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Channel ref to deliver on (e.g. 'telegram.prod'). Must be one of the agent's configured channels and a channel the target peer also listens on."
                },
                "target": {
                    "type": "string",
                    "description": "Recipient identifier — a peer agent's alias or an external peer's username (e.g. '@operator')."
                },
                "message": {
                    "type": "string",
                    "description": "The message body to deliver."
                }
            },
            "required": ["channel", "target", "message"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let channel = args
            .get("channel")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing or empty 'channel' parameter"))?
            .to_string();
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing or empty 'target' parameter"))?
            .to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing or empty 'message' parameter"))?
            .to_string();

        let channel_ref = ChannelRef::from(channel.as_str());
        let resolved = resolve_peer_set(&self.config, &self.sender_alias);

        if !resolved.is_known_peer(&channel_ref, &target) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "target {target:?} is not on agent {alias:?}'s resolved peer set for channel {channel:?}; \
                     add a [peer_groups.<name>] entry that lists both this agent and the target before sending",
                    alias = self.sender_alias,
                )),
            });
        }

        // The agent must itself listen on the channel — the target may
        // be reachable on it via a peer group, but a sender can't
        // dispatch on a channel it isn't configured for.
        let agent_listens_on_channel = self
            .config
            .agents
            .get(&self.sender_alias)
            .map(|a| a.channels.iter().any(|c| c.as_str() == channel.as_str()))
            .unwrap_or(false);
        if !agent_listens_on_channel {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "agent {alias:?} does not list channel {channel:?} on its `channels`; \
                     add the channel ref to [agents.{alias}.channels] before sending",
                    alias = self.sender_alias,
                )),
            });
        }

        match deliver_announcement(&self.config, &channel, &target, &message).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("delivered to {target} on {channel}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("delivery failed: {e:#}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::multi_agent::{AgentAlias, PeerExternal, PeerGroupConfig, PeerUsername};
    use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    fn config_with_two_agents_and_one_peer_group() -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".into(), RiskProfileConfig::default());
        for alias in ["alpha", "beta"] {
            let mut agent = AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            };
            agent.channels.push(ChannelRef::from("telegram.prod"));
            config.agents.insert(alias.to_string(), agent);
        }
        config.peer_groups.insert(
            "research".into(),
            PeerGroupConfig {
                channel: ChannelRef::from("telegram.prod"),
                agents: vec![AgentAlias::from("alpha"), AgentAlias::from("beta")],
                external_peers: vec![PeerExternal {
                    username: PeerUsername::from("operator"),
                }],
                ignore: vec![],
            },
        );
        config
    }

    #[tokio::test]
    async fn rejects_target_not_on_resolved_peer_set() {
        let cfg = Arc::new(config_with_two_agents_and_one_peer_group());
        let tool = SendMessageToPeerTool::new(cfg, "alpha");
        // "stranger" is on no peer group with alpha → reject.
        let result = tool
            .execute(json!({
                "channel": "telegram.prod",
                "target": "stranger",
                "message": "hi"
            }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not on agent")
        );
    }

    #[tokio::test]
    async fn rejects_send_on_channel_agent_does_not_listen_on() {
        let mut cfg = config_with_two_agents_and_one_peer_group();
        // Drop alpha's channels so it doesn't listen on telegram.prod.
        cfg.agents.get_mut("alpha").expect("alpha").channels.clear();
        // But the resolver still computes a peer set from peer_groups —
        // simulate the misconfig where the validator missed it.
        let tool = SendMessageToPeerTool::new(Arc::new(cfg), "alpha");
        let result = tool
            .execute(json!({
                "channel": "telegram.prod",
                "target": "beta",
                "message": "hi"
            }))
            .await
            .expect("execute returns Ok with structured failure");
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        // Either "does not list channel" (channel-listener guard) or
        // "not on agent ... resolved peer set" (resolver guard) is a
        // valid rejection — both refuse the send safely.
        assert!(
            err.contains("does not list channel") || err.contains("not on agent"),
            "expected channel-listener or peer-set rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn empty_args_are_rejected() {
        let cfg = Arc::new(config_with_two_agents_and_one_peer_group());
        let tool = SendMessageToPeerTool::new(cfg, "alpha");
        for args in [
            json!({}),
            json!({ "channel": "", "target": "beta", "message": "hi" }),
            json!({ "channel": "telegram.prod", "target": "  ", "message": "hi" }),
            json!({ "channel": "telegram.prod", "target": "beta", "message": "" }),
        ] {
            tool.execute(args)
                .await
                .expect_err("missing/empty arg must fail");
        }
    }

    #[tokio::test]
    async fn accepts_external_peer_with_at_prefix_normalization() {
        // The external peer is stored as "operator" (no @, lowercase);
        // inbound handles often arrive as "@Operator". The peer-set
        // check must accept the normalized match. Delivery itself will
        // fail because no `DELIVERY_FN` is registered in unit tests,
        // but that's a different layer than the peer-set authorization
        // surface this test covers.
        let cfg = Arc::new(config_with_two_agents_and_one_peer_group());
        let tool = SendMessageToPeerTool::new(cfg, "alpha");
        let result = tool
            .execute(json!({
                "channel": "telegram.prod",
                "target": "@Operator",
                "message": "hi"
            }))
            .await
            .expect("execute returns Ok with structured failure");
        // We expect the peer-set check to PASS (so the error mentions
        // delivery, not the peer set).
        if !result.success {
            let err = result.error.unwrap_or_default();
            assert!(
                err.contains("delivery") || err.contains("not registered"),
                "expected delivery-layer error after peer-set passes, got: {err}"
            );
        }
    }
}
