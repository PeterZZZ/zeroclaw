//! Conversation transcript serialization to Claude Code JSONL format.
//!
//! Bridges ZeroClaw's internal [`ConversationMessage`] schema to the line-
//! delimited JSON shape consumed by external tooling that originated in the
//! Claude Code ecosystem (e.g. MemSkills' `procedure-add` pipeline). The
//! output format mirrors Claude Code's session transcript files:
//!
//! Each line is a JSON object with:
//! - `type`: `"user"` | `"assistant"` | `"system"`
//! - `message`: `{ "role": ..., "content": [<content_block>, ...] }`
//!
//! Content blocks are one of:
//! - `{ "type": "text", "text": "..." }`
//! - `{ "type": "tool_use", "id": "...", "name": "...", "input": {...} }`
//! - `{ "type": "tool_result", "tool_use_id": "...", "content": "..." }`
//!
//! Tool results are wrapped inside a user-role message to match Claude Code's
//! convention of feeding tool output back through the user channel.

use serde_json::{Map, Value, json};

use crate::provider::ConversationMessage;

/// Serialize a slice of [`ConversationMessage`] into Claude Code JSONL.
///
/// Returns one JSON object per line, terminated by `\n`. Empty input
/// produces an empty string. Never fails — invalid tool-call argument JSON
/// is preserved as a raw string in the `input` field.
pub fn serialize_history_to_jsonl(history: &[ConversationMessage]) -> String {
    let mut out = String::new();
    for msg in history {
        let value = message_to_jsonl_entry(msg);
        if let Ok(line) = serde_json::to_string(&value) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Map a single `ConversationMessage` to a Claude-Code-shaped JSONL entry.
fn message_to_jsonl_entry(msg: &ConversationMessage) -> Value {
    match msg {
        ConversationMessage::Chat(chat) => {
            let entry_type = match chat.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "system" => "system",
                // Tool-role messages in the legacy ChatMessage form get
                // mapped onto a user-role tool_result envelope so downstream
                // scanners that look for tool results in user blocks still
                // see them.
                "tool" => "user",
                other => other,
            };
            let content_block = json!({"type": "text", "text": chat.content});
            json!({
                "type": entry_type,
                "message": {
                    "role": chat.role,
                    "content": [content_block],
                }
            })
        }
        ConversationMessage::AssistantToolCalls {
            text,
            tool_calls,
            reasoning_content: _,
        } => {
            let mut content = Vec::with_capacity(tool_calls.len() + 1);
            if let Some(text) = text
                && !text.is_empty()
            {
                content.push(json!({"type": "text", "text": text}));
            }
            for call in tool_calls {
                // Try to parse arguments as JSON; if it fails, pass through
                // as a raw string so the line stays well-formed.
                let input: Value = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                content.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": content,
                }
            })
        }
        ConversationMessage::ToolResults(results) => {
            let content: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": r.tool_call_id,
                        "content": r.content,
                    })
                })
                .collect();
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": content,
                }
            })
        }
    }
}

/// Helper: build a header line some Claude Code transcripts include with
/// session metadata. Optional — emit only if the caller wants a self-
/// describing transcript. Not part of the message stream.
pub fn build_transcript_header(session_id: &str, channel: &str, cwd: &str) -> String {
    let mut map = Map::new();
    map.insert("type".into(), Value::String("summary".into()));
    map.insert("session_id".into(), Value::String(session_id.into()));
    map.insert("channel".into(), Value::String(channel.into()));
    map.insert("cwd".into(), Value::String(cwd.into()));
    let value = Value::Object(map);
    let mut s = serde_json::to_string(&value).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ToolCall, ToolResultMessage};

    fn parse_lines(s: &str) -> Vec<Value> {
        s.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("valid JSON"))
            .collect()
    }

    #[test]
    fn empty_history_produces_empty_string() {
        let s = serialize_history_to_jsonl(&[]);
        assert!(s.is_empty());
    }

    #[test]
    fn user_chat_message_becomes_user_text_block() {
        let history = vec![ConversationMessage::Chat(ChatMessage::user("hello"))];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["message"]["role"], "user");
        assert_eq!(lines[0]["message"]["content"][0]["type"], "text");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_chat_message_becomes_assistant_text_block() {
        let history = vec![ConversationMessage::Chat(ChatMessage::assistant("hi"))];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["role"], "assistant");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "hi");
    }

    #[test]
    fn system_message_preserves_role() {
        let history = vec![ConversationMessage::Chat(ChatMessage::system("be brief"))];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines[0]["type"], "system");
        assert_eq!(lines[0]["message"]["role"], "system");
    }

    #[test]
    fn assistant_tool_calls_emit_text_and_tool_use_blocks() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("running".into()),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "Bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                extra_content: None,
            }],
            reasoning_content: None,
        }];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines.len(), 1);
        let content = lines[0]["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "running");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "call_1");
        assert_eq!(content[1]["name"], "Bash");
        assert_eq!(content[1]["input"]["command"], "ls");
    }

    #[test]
    fn assistant_tool_calls_with_no_text_omits_text_block() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
                extra_content: None,
            }],
            reasoning_content: None,
        }];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        let content = lines[0]["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
    }

    #[test]
    fn tool_results_become_user_role_tool_result_blocks() {
        let history = vec![ConversationMessage::ToolResults(vec![
            ToolResultMessage {
                tool_call_id: "call_1".into(),
                content: "file contents".into(),
            },
        ])];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines[0]["type"], "user");
        let content = lines[0]["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_1");
        assert_eq!(content[0]["content"], "file contents");
    }

    #[test]
    fn invalid_tool_arguments_passed_as_raw_string() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "Bash".into(),
                arguments: "not-json".into(),
                extra_content: None,
            }],
            reasoning_content: None,
        }];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        let input = &lines[0]["message"]["content"][0]["input"];
        assert_eq!(input, &Value::String("not-json".into()));
    }

    #[test]
    fn multi_turn_history_preserves_order() {
        let history = vec![
            ConversationMessage::Chat(ChatMessage::user("/procedure-add this")),
            ConversationMessage::Chat(ChatMessage::assistant("ok")),
        ];
        let lines = parse_lines(&serialize_history_to_jsonl(&history));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["message"]["content"][0]["text"], "/procedure-add this");
        assert_eq!(lines[1]["message"]["content"][0]["text"], "ok");
    }

    #[test]
    fn each_message_serialized_as_one_line_terminated_by_newline() {
        let history = vec![
            ConversationMessage::Chat(ChatMessage::user("a")),
            ConversationMessage::Chat(ChatMessage::user("b")),
        ];
        let s = serialize_history_to_jsonl(&history);
        assert_eq!(s.matches('\n').count(), 2);
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn build_transcript_header_includes_metadata() {
        let header = build_transcript_header("session_abc", "cli", "/tmp/work");
        let v: Value = serde_json::from_str(header.trim()).unwrap();
        assert_eq!(v["type"], "summary");
        assert_eq!(v["session_id"], "session_abc");
        assert_eq!(v["channel"], "cli");
        assert_eq!(v["cwd"], "/tmp/work");
    }
}
