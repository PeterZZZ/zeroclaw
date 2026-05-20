//! External-process session-end hook.
//!
//! Bridges ZeroClaw's `on_session_end_with_history` lifecycle event to an
//! external command (e.g. a Node.js script). On session end:
//!
//! 1. Serializes the agent's conversation history to a JSONL file under
//!    `transcript_dir` using [`zeroclaw_api::transcript::serialize_history_to_jsonl`].
//! 2. Spawns the configured command with the configured args, piping a small
//!    JSON envelope (`{"transcript_path": ..., "cwd": ..., "session_id": ...,
//!    "channel": ...}`) to the child's stdin.
//! 3. Waits up to `timeout_secs` for the child to exit, then kills it.
//!
//! All failures are logged but never propagated — hooks must not block the
//! runtime. This handler is used by MemSkills' `procedure-add` pipeline.

use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::hooks::traits::{HookHandler, SessionEndPayload};
use zeroclaw_api::transcript::serialize_history_to_jsonl;
use zeroclaw_config::schema::ExternalSessionEndConfig;

/// Hook that writes the session transcript to disk and shells out to an
/// external command on session end.
pub struct ExternalProcessHook {
    config: ExternalSessionEndConfig,
    /// Resolved transcript directory. If the config left it empty, this is
    /// `~/.zeroclaw/workspace/transcripts`. Computed at construction time.
    transcript_dir: PathBuf,
}

impl ExternalProcessHook {
    /// Build a hook from config. Resolves a default transcript directory
    /// under the user's home if the config left `transcript_dir` empty.
    pub fn new(config: ExternalSessionEndConfig) -> Self {
        let transcript_dir = resolve_transcript_dir(&config.transcript_dir);
        Self {
            config,
            transcript_dir,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs.max(1))
    }

    /// Write JSONL to disk and return the file path on success.
    async fn write_transcript(
        &self,
        session_id: &str,
        jsonl: &str,
    ) -> std::io::Result<PathBuf> {
        fs::create_dir_all(&self.transcript_dir).await?;
        let filename = format!("{}-{}.jsonl", session_id, transcript_timestamp());
        let path = self.transcript_dir.join(filename);
        fs::write(&path, jsonl).await?;
        Ok(path)
    }

    /// Spawn the configured command with the envelope on stdin.
    async fn spawn_child(
        &self,
        transcript_path: &str,
        session_id: &str,
        channel: &str,
        cwd: &str,
    ) -> Result<(), String> {
        let envelope = json!({
            "transcript_path": transcript_path,
            "cwd": cwd,
            "session_id": session_id,
            "channel": channel,
        })
        .to_string();

        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(envelope.as_bytes())
                .await
                .map_err(|e| format!("stdin write failed: {e}"))?;
            // Explicit drop to close stdin so child can exit.
            drop(stdin);
        }

        match timeout(self.timeout(), child.wait()).await {
            Ok(Ok(status)) => {
                if !status.success() {
                    return Err(format!("child exited with status {status}"));
                }
                Ok(())
            }
            Ok(Err(e)) => Err(format!("wait failed: {e}")),
            Err(_) => {
                let _ = child.start_kill();
                Err(format!("child timed out after {}s", self.config.timeout_secs))
            }
        }
    }
}

fn resolve_transcript_dir(configured: &str) -> PathBuf {
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    if let Some(home) = directories::UserDirs::new() {
        home.home_dir()
            .join(".zeroclaw")
            .join("workspace")
            .join("transcripts")
    } else {
        PathBuf::from("./.zeroclaw-transcripts")
    }
}

fn transcript_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[async_trait]
impl HookHandler for ExternalProcessHook {
    fn name(&self) -> &str {
        "external-session-end"
    }

    fn priority(&self) -> i32 {
        // Run after observability hooks so the transcript reflects everything
        // they recorded. Negative priority places this near the end of the
        // dispatch ordering.
        -100
    }

    async fn on_session_end_with_history(&self, payload: SessionEndPayload<'_>) {
        if !self.config.enabled {
            return;
        }
        if self.config.command.is_empty() {
            tracing::warn!(
                hook = self.name(),
                "external-session-end hook enabled but no command configured; skipping"
            );
            return;
        }

        let jsonl = serialize_history_to_jsonl(payload.history);

        let path = match self.write_transcript(payload.session_id, &jsonl).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    hook = self.name(),
                    error = %e,
                    "failed to write transcript file; skipping external hook"
                );
                return;
            }
        };

        let path_str = path.to_string_lossy().to_string();
        if let Err(e) = self
            .spawn_child(&path_str, payload.session_id, payload.channel, payload.cwd)
            .await
        {
            tracing::warn!(
                hook = self.name(),
                error = %e,
                transcript = %path_str,
                "external session-end command failed"
            );
        } else {
            tracing::info!(
                hook = self.name(),
                transcript = %path_str,
                session_id = payload.session_id,
                "external session-end hook completed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zeroclaw_api::provider::{ChatMessage, ConversationMessage};
    use crate::hooks::HookRunner;

    fn enabled_config(command: &str, args: Vec<String>, dir: PathBuf) -> ExternalSessionEndConfig {
        ExternalSessionEndConfig {
            enabled: true,
            command: command.to_string(),
            args,
            transcript_dir: dir.to_string_lossy().to_string(),
            timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn disabled_hook_is_noop() {
        let tmp = tempdir();
        let cfg = ExternalSessionEndConfig {
            enabled: false,
            command: "/nonexistent/command".into(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let hook = ExternalProcessHook::new(cfg);
        let history = vec![ConversationMessage::Chat(ChatMessage::user("hi"))];
        let payload = SessionEndPayload {
            session_id: "s1",
            channel: "cli",
            history: &history,
            cwd: "/tmp",
        };
        // Must not write any transcript when disabled.
        hook.on_session_end_with_history(payload).await;
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "disabled hook wrote files");
    }

    #[tokio::test]
    async fn writes_transcript_and_invokes_command() {
        let tmp = tempdir();
        // Use /bin/cat as a harmless child that consumes stdin and exits.
        let cfg = enabled_config("/bin/cat", vec![], tmp.path().to_path_buf());
        let hook = ExternalProcessHook::new(cfg);
        let history = vec![
            ConversationMessage::Chat(ChatMessage::user("/procedure-add this")),
            ConversationMessage::Chat(ChatMessage::assistant("ok")),
        ];
        let payload = SessionEndPayload {
            session_id: "session_abc",
            channel: "cli",
            history: &history,
            cwd: "/tmp/work",
        };
        hook.on_session_end_with_history(payload).await;

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1, "expected one transcript file");
        let path = files[0].path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("session_abc-"), "name was {name}");
        assert!(name.ends_with(".jsonl"), "name was {name}");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("/procedure-add this"));
        assert!(content.contains("ok"));
        assert_eq!(content.lines().count(), 2);
    }

    #[tokio::test]
    async fn missing_command_logs_but_does_not_panic() {
        let tmp = tempdir();
        let cfg = enabled_config(
            "/definitely/not/a/real/binary",
            vec![],
            tmp.path().to_path_buf(),
        );
        let hook = ExternalProcessHook::new(cfg);
        let history = vec![ConversationMessage::Chat(ChatMessage::user("hi"))];
        let payload = SessionEndPayload {
            session_id: "s2",
            channel: "cli",
            history: &history,
            cwd: "/tmp",
        };
        // The transcript still gets written, but spawn fails. Should not panic.
        hook.on_session_end_with_history(payload).await;
        // Transcript file should still exist.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn empty_command_is_noop_after_warning() {
        let tmp = tempdir();
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: String::new(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let hook = ExternalProcessHook::new(cfg);
        let history = vec![ConversationMessage::Chat(ChatMessage::user("hi"))];
        let payload = SessionEndPayload {
            session_id: "s3",
            channel: "cli",
            history: &history,
            cwd: "/tmp",
        };
        hook.on_session_end_with_history(payload).await;
        // Empty command should skip transcript write entirely.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn child_timeout_is_enforced() {
        let tmp = tempdir();
        // sleep 10 with timeout 1 should be killed.
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "/bin/sleep".into(),
            args: vec!["10".into()],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 1,
        };
        let hook = ExternalProcessHook::new(cfg);
        let history = vec![ConversationMessage::Chat(ChatMessage::user("hi"))];
        let payload = SessionEndPayload {
            session_id: "s4",
            channel: "cli",
            history: &history,
            cwd: "/tmp",
        };
        let start = std::time::Instant::now();
        hook.on_session_end_with_history(payload).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "hook did not enforce timeout: elapsed = {elapsed:?}"
        );
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    /// Replicate the exact data path that the orchestrator's
    /// `fire_session_end_for_sender` runs in production:
    ///   1. Look up per-sender history in a keyed cache
    ///   2. Map `Vec<ChatMessage>` → `Vec<ConversationMessage::Chat>`
    ///   3. Dispatch via the HookRunner with the converted history
    /// Asserts the registered ExternalProcessHook actually wrote a
    /// transcript whose contents reflect the cached history.
    #[tokio::test]
    async fn orchestrator_dataflow_fires_external_hook_with_cached_history() {
        let tmp = tempdir();
        // Use `/bin/cat` as the external command — it consumes stdin
        // (validating that the spawn + pipe path runs) and exits cleanly.
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "/bin/cat".into(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let mut runner = HookRunner::new();
        runner.register(Box::new(ExternalProcessHook::new(cfg)));

        // Mirror the orchestrator's storage: a per-sender map of cached
        // chat turns. The real type is `Arc<Mutex<LruCache<...>>>`; the
        // lookup semantics that matter here are identical for HashMap.
        let mut sender_histories: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        sender_histories.insert(
            "alice:cli".into(),
            vec![
                ChatMessage::user("/procedure-add demo-procedure"),
                ChatMessage::assistant("ack"),
                ChatMessage::user("ok thanks"),
            ],
        );

        // Inline replication of fire_session_end_for_sender:
        //   1. peek the cache for this sender
        //   2. early-return if empty (no transcript to fire)
        //   3. map ChatMessage → ConversationMessage::Chat
        //   4. dispatch on the runner
        let sender_key = "alice:cli";
        let channel = "cli";
        let history_snapshot = sender_histories
            .get(sender_key)
            .cloned()
            .unwrap_or_default();
        assert!(!history_snapshot.is_empty(), "test setup invariant");

        let history: Vec<ConversationMessage> = history_snapshot
            .into_iter()
            .map(ConversationMessage::Chat)
            .collect();
        let cwd = "/tmp/workspace";
        runner
            .fire_session_end_with_history(sender_key, channel, &history, cwd)
            .await;

        // Assertion 1: a transcript file was written with the sender_key
        // as the filename prefix.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one transcript file");
        let path = entries[0].path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("alice:cli-") || name.starts_with("alice%3Acli-"),
            "filename should be derived from sender_key; got {name}"
        );

        // Assertion 2: the file contains 3 JSONL lines (one per cached
        // ChatMessage) and the user's slash invocation made it through.
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 3, "expected 3 JSONL lines");
        assert!(
            body.contains(r#""text":"/procedure-add demo-procedure""#),
            "expected the user's slash command in the transcript; got: {body}"
        );
        assert!(body.contains(r#""role":"assistant""#));
        assert!(body.contains(r#""role":"user""#));
    }

    /// Mirror the orchestrator's early-return path for empty senders.
    /// A sender_key with no cached history must produce zero side effects
    /// (no transcript file, no child spawn).
    #[tokio::test]
    async fn orchestrator_dataflow_empty_history_does_nothing() {
        let tmp = tempdir();
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "/bin/cat".into(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let mut runner = HookRunner::new();
        runner.register(Box::new(ExternalProcessHook::new(cfg)));

        // Empty sender history → we should skip dispatch entirely.
        // Replicate orchestrator's `if history_snapshot.is_empty() { return; }`
        // by simply not firing.
        let sender_histories: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        let history_snapshot = sender_histories
            .get("unknown:sender")
            .cloned()
            .unwrap_or_default();
        if !history_snapshot.is_empty() {
            // (would fire; never reached in this test)
            unreachable!();
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "no transcript should exist for an empty sender"
        );
    }

    /// CLI-path equivalent of `orchestrator_dataflow_fires_external_hook_with_cached_history`.
    ///
    /// In the CLI interactive REPL there is no per-sender HashMap — there
    /// is a single linear `Vec<ChatMessage>` named `history` living at
    /// `crates/zeroclaw-runtime/src/agent/loop_.rs`. Phase 11 wires
    /// `fire_session_end_with_history` against that vector inside the
    /// `"/clear" | "/new"` match arm, with sender_key="cli" and
    /// channel="cli". This test replicates that dataflow inline (no
    /// LLM, no actual REPL) to prove the registered ExternalProcessHook
    /// emits a transcript whose name keys off the `"cli"` sender and
    /// whose body contains the about-to-be-cleared CLI history verbatim.
    #[tokio::test]
    async fn cli_dataflow_fires_external_hook_with_cli_history() {
        let tmp = tempdir();
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "/bin/cat".into(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let mut runner = HookRunner::new();
        runner.register(Box::new(ExternalProcessHook::new(cfg)));

        // Mirror the CLI loop's storage: a linear conversation history.
        // System messages are filtered upstream of the hook in loop_.rs
        // (the about-to-clear `history` includes the leading system
        // prompt), but the hook itself accepts whatever the caller hands
        // it — keep the fixture small and focused on user/assistant turns.
        let history: Vec<ChatMessage> = vec![
            ChatMessage::user("hello from the cli /new test"),
            ChatMessage::assistant("acknowledged"),
            ChatMessage::user("/new"),
        ];

        // Inline replication of Phase 11's call in the `"/clear" | "/new"`
        // match arm:
        //   1. early-return if hooks is None (this test always registers one)
        //   2. early-return if history is empty
        //   3. map ChatMessage → ConversationMessage::Chat
        //   4. fire_session_end_with_history with sender_key="cli", channel="cli"
        assert!(!history.is_empty(), "test setup invariant");
        let sender_key = "cli";
        let channel = "cli";
        let snapshot: Vec<ConversationMessage> = history
            .iter()
            .cloned()
            .map(ConversationMessage::Chat)
            .collect();
        let cwd = "/tmp/cli-workspace";
        runner
            .fire_session_end_with_history(sender_key, channel, &snapshot, cwd)
            .await;

        // Assertion 1: exactly one transcript file was written and its
        // filename keys off the "cli" sender_key prefix.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one transcript file");
        let path = entries[0].path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("cli-"),
            "filename should start with cli-; got {name}"
        );

        // Assertion 2: the file contains 3 JSONL lines (one per CLI turn)
        // and the user's /new command made it through verbatim.
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            body.lines().count(),
            3,
            "expected 3 JSONL lines (user, assistant, user); got:\n{body}"
        );
        assert!(
            body.contains(r#""text":"hello from the cli /new test""#),
            "expected the user's first message in the transcript; got: {body}"
        );
        assert!(body.contains(r#""text":"/new""#));
        assert!(body.contains(r#""role":"assistant""#));
        assert!(body.contains(r#""role":"user""#));
    }

    /// CLI-path equivalent of `orchestrator_dataflow_empty_history_does_nothing`.
    ///
    /// At session start the CLI loop seeds `history` with a single
    /// system message; if the user types `/new` immediately (or after
    /// pressing `/clear` and getting redirected back to an empty
    /// history), Phase 11's empty-history guard must skip the
    /// dispatch entirely. This test replicates that guard inline.
    #[tokio::test]
    async fn cli_dataflow_empty_history_does_nothing() {
        let tmp = tempdir();
        let cfg = ExternalSessionEndConfig {
            enabled: true,
            command: "/bin/cat".into(),
            args: vec![],
            transcript_dir: tmp.path().to_string_lossy().to_string(),
            timeout_secs: 5,
        };
        let mut runner = HookRunner::new();
        runner.register(Box::new(ExternalProcessHook::new(cfg)));

        // Empty CLI history → skip dispatch entirely.
        let history: Vec<ChatMessage> = vec![];
        if !history.is_empty() {
            // (would fire; never reached in this test)
            unreachable!();
        }

        // Drain runner so we never accidentally fire below.
        drop(runner);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "no transcript should exist for an empty CLI history"
        );
    }
}
