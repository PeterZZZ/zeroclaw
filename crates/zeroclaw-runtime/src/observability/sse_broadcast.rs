//! SSE broadcast observer for use by runtime subsystems (cron, heartbeat).
//!
//! Unlike the gateway's `BroadcastObserver`, this implementation has no inner
//! observer wrapper and no history ring-buffer — it is a thin adapter that
//! maps `ObserverEvent`s to JSON and sends them to the shared SSE broadcast
//! channel. The gateway's `BroadcastObserver` handles buffering and inner-
//! observer delegation for the gateway-chat path.

use tokio::sync::broadcast::Sender;

use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};

/// Lightweight observer that forwards lifecycle events to the SSE event bus.
///
/// Intended for use by runtime subsystems (cron scheduler, heartbeat worker)
/// that hold a reference to the shared `event_tx` broadcast sender but do not
/// have access to the gateway's ring buffer or inner observer chain.
pub struct SseBroadcastObserver {
    tx: Sender<serde_json::Value>,
}

impl SseBroadcastObserver {
    pub fn new(tx: Sender<serde_json::Value>) -> Self {
        Self { tx }
    }
}

impl Observer for SseBroadcastObserver {
    fn record_event(&self, event: &ObserverEvent) {
        let json = match event {
            ObserverEvent::AgentStart { provider, model } => serde_json::json!({
                "type": "agent_start",
                "provider": provider,
                "model": model,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::AgentEnd {
                provider,
                model,
                duration,
                tokens_used,
                cost_usd,
            } => serde_json::json!({
                "type": "agent_end",
                "provider": provider,
                "model": model,
                "duration_ms": duration.as_millis(),
                "tokens_used": tokens_used,
                "cost_usd": cost_usd,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::LlmRequest {
                provider, model, ..
            } => serde_json::json!({
                "type": "llm_request",
                "provider": provider,
                "model": model,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::LlmResponse {
                provider,
                model,
                duration,
                success,
                ..
            } => serde_json::json!({
                "type": "llm_response",
                "provider": provider,
                "model": model,
                "duration_ms": duration.as_millis(),
                "success": success,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::ToolCallStart { tool, .. } => serde_json::json!({
                "type": "tool_call_start",
                "tool": tool,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
            } => serde_json::json!({
                "type": "tool_call",
                "tool": tool,
                "duration_ms": duration.as_millis(),
                "success": success,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::TurnComplete => serde_json::json!({
                "type": "turn_complete",
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::ChannelMessage { channel, direction } => serde_json::json!({
                "type": "channel_message",
                "channel": channel,
                "direction": direction,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::HeartbeatTick => serde_json::json!({
                "type": "heartbeat_tick",
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ObserverEvent::Error { component, message } => serde_json::json!({
                "type": "error",
                "component": component,
                "message": message,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            _ => return,
        };
        let _ = self.tx.send(json);
    }

    fn record_metric(&self, _metric: &ObserverMetric) {
        // Metrics are not forwarded over SSE; use Prometheus or OTel for that.
    }

    fn name(&self) -> &str {
        "sse-broadcast"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
