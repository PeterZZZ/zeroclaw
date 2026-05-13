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
    use zeroclaw_api::provider::{ChatMessage, ConversationMessage};

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
}
