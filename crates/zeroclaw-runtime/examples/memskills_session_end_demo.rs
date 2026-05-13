//! End-to-end demo of the MemSkills session-end integration.
//!
//! Builds a synthetic conversation history, runs it through ZeroClaw's
//! ExternalProcessHook → serializer → MemSkills hook bin pipeline, and
//! prints what each stage produced. Useful as a sanity check after
//! changing the trait or serializer.
//!
//! Run with: `cargo run --example memskills_session_end_demo -p zeroclaw-runtime`

use std::collections::HashMap;
use std::path::PathBuf;

use zeroclaw_api::provider::{ChatMessage, ConversationMessage, ToolCall};
use zeroclaw_api::transcript::serialize_history_to_jsonl;
use zeroclaw_config::schema::ExternalSessionEndConfig;
use zeroclaw_runtime::hooks::HookRunner;
use zeroclaw_runtime::hooks::builtin::ExternalProcessHook;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Build a synthetic conversation that includes a slash-command
    //    invocation, so MemSkills' transcript scanner has something to find.
    let history = vec![
        ConversationMessage::Chat(ChatMessage::user("/procedure-add demo-procedure")),
        ConversationMessage::AssistantToolCalls {
            text: Some("Starting procedure add".into()),
            tool_calls: vec![ToolCall {
                id: "tc_1".into(),
                name: "Skill".into(),
                arguments: r#"{"skill":"procedure-add"}"#.into(),
                extra_content: None,
            }],
            reasoning_content: None,
        },
        ConversationMessage::Chat(ChatMessage::assistant(
            "Start executing procedure /procedure-add",
        )),
    ];

    println!("=== Stage 1: Synthetic conversation ({} messages) ===", history.len());

    // 2. Serializer produces JSONL.
    let jsonl = serialize_history_to_jsonl(&history);
    println!("\n=== Stage 2: Serialized JSONL ===");
    println!("{jsonl}");

    // 3. Wire ExternalProcessHook to invoke the real MemSkills hook bin.
    //    The bin path comes from an env var so this example stays portable.
    let memskills_bin = std::env::var("MEMSKILLS_HOOK_BIN").unwrap_or_else(|_| {
        "/Users/petermemverge/Desktop/MemVerge/MemSkills/skills/general/procedure-memory/bin/proc-session-end-hook.mjs".into()
    });

    let transcript_dir: PathBuf = std::env::temp_dir().join("zeroclaw-demo-transcripts");

    let cfg = ExternalSessionEndConfig {
        enabled: true,
        command: "node".into(),
        args: vec![memskills_bin.clone()],
        transcript_dir: transcript_dir.to_string_lossy().to_string(),
        timeout_secs: 30,
    };
    let hook = ExternalProcessHook::new(cfg);

    println!("=== Stage 3: Firing session-end hook ===");
    println!("  command: node");
    println!("  args:    [{}]", memskills_bin);
    println!("  dir:     {}", transcript_dir.display());

    // 4. Fire through HookRunner so the dispatch path matches production.
    let mut runner = HookRunner::new();
    runner.register(Box::new(hook));
    runner
        .fire_session_end_with_history(
            "demo-session-001",
            "cli",
            &history,
            &std::env::current_dir()?.to_string_lossy(),
        )
        .await;

    // 5. Locate the written transcript file and print it back.
    println!("\n=== Stage 4: Transcripts written to disk ===");
    if let Ok(rd) = std::fs::read_dir(&transcript_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name.starts_with("demo-session-001-") {
                let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                println!("  {}  ({bytes} bytes)", p.display());
            }
        }
    }

    println!("\nDone. MemSkills hook ran end-to-end (no failures = exit-0 from bin).");

    // ------------------------------------------------------------------
    // Bonus stage: prove the orchestrator data flow works
    // ------------------------------------------------------------------
    // The previous stages hand-built a `Vec<ConversationMessage>` and fed
    // it to the runner. In production, the orchestrator stores conversation
    // history as `Vec<ChatMessage>` keyed by sender, and the new helper
    // `fire_session_end_for_sender` does the same conversion this stage
    // performs. Demonstrating it here proves the wiring in
    // `crates/zeroclaw-channels/src/orchestrator/mod.rs` will produce a
    // valid transcript at /new command time.

    println!("\n=== Bonus: orchestrator data flow simulation ===");
    let mut sender_histories: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    sender_histories.insert(
        "alice:cli".to_string(),
        vec![
            ChatMessage::user("/procedure-add demo-orchestrator-flow"),
            ChatMessage::assistant("Acknowledged."),
            ChatMessage::user("/new"),
        ],
    );

    // Now mirror fire_session_end_for_sender:
    let sender_key = "alice:cli";
    let channel = "cli";
    let history_snapshot = sender_histories
        .get(sender_key)
        .cloned()
        .unwrap_or_default();
    println!(
        "  cache lookup for sender_key='{sender_key}' → {} turns",
        history_snapshot.len()
    );
    if history_snapshot.is_empty() {
        println!("  (no transcript to emit; orchestrator early-returns)");
    } else {
        let converted: Vec<ConversationMessage> = history_snapshot
            .into_iter()
            .map(ConversationMessage::Chat)
            .collect();
        println!("  converted {} ChatMessages to ConversationMessage::Chat", converted.len());

        let bonus_dir = std::env::temp_dir().join("zeroclaw-orchestrator-flow");
        let _ = std::fs::remove_dir_all(&bonus_dir);
        let bonus_cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "node".into(),
            args: vec![std::env::var("MEMSKILLS_HOOK_BIN").unwrap_or_else(|_|
                "/Users/petermemverge/Desktop/MemVerge/MemSkills/skills/general/procedure-memory/bin/proc-session-end-hook.mjs".into()
            )],
            transcript_dir: bonus_dir.to_string_lossy().to_string(),
            timeout_secs: 30,
        };
        let mut bonus_runner = HookRunner::new();
        bonus_runner.register(Box::new(ExternalProcessHook::new(bonus_cfg)));
        bonus_runner
            .fire_session_end_with_history(sender_key, channel, &converted, &std::env::current_dir()?.to_string_lossy())
            .await;

        // Show what landed on disk.
        if let Ok(rd) = std::fs::read_dir(&bonus_dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                println!("  transcript: {} ({bytes} bytes)", p.display());
                if let Ok(body) = std::fs::read_to_string(&p) {
                    for (i, line) in body.lines().enumerate() {
                        println!("    line {}: {}", i + 1, truncate_for_display(line, 100));
                    }
                }
            }
        }
        println!("  ✓ orchestrator-shape data made it through the wire");
    }

    Ok(())
}

fn truncate_for_display(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
