//! Per-agent lifecycle primitives: create, delete, list.
//!
//! These functions are the runtime-layer capabilities the operator
//! surface (a future `zeroclaw agents` CLI / web admin / gateway
//! endpoint) calls. They keep the on-disk shape consistent across
//! every entry point: write the `[agents.<alias>]` config block,
//! create the per-agent workspace dir, seed bootstrap identity files,
//! and atomically save the config; or strip the block, remove the
//! dir, and rewrite peer-group memberships in one save.
//!
//! The session registry (this module's [`session_registry`]) is the
//! gate that makes [`delete_agent`] safe under load: a delete refuses
//! when an alias has active sessions unless the caller explicitly
//! passes `force_active_sessions = true`.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use zeroclaw_config::multi_agent::MemoryBackendKind;
use zeroclaw_config::schema::{AliasedAgentConfig, Config, ensure_bootstrap_files};

pub mod session_registry;

/// Inputs for [`create_agent`].
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Unique alias under `[agents.<alias>]`. Must not already exist.
    pub alias: String,
    /// Risk-profile alias the agent inherits. Must reference a configured
    /// `[risk_profiles.<name>]` entry.
    pub risk_profile: String,
    /// Optional model-provider dotted alias (e.g. `openrouter.default`).
    /// `None` keeps the `AliasedAgentConfig::default()` empty value.
    pub model_provider: Option<String>,
    /// Optional memory backend kind. `None` keeps the default
    /// (`MemoryBackendKind::Sqlite`).
    pub memory_backend: Option<MemoryBackendKind>,
}

/// Inputs for [`delete_agent`].
#[derive(Debug, Clone, Default)]
pub struct DeleteOptions {
    /// When `true`, compute the impact report without changing on-disk
    /// state. The returned [`DeleteReport`] reflects what *would* be
    /// removed; nothing is touched.
    pub dry_run: bool,
    /// When `true`, proceed even if [`session_registry::active_sessions_for`]
    /// reports active runs for the alias. Default refuses with an
    /// error so an in-flight agent can't have its config swept out.
    pub force_active_sessions: bool,
}

/// Per-call summary of a [`delete_agent`] invocation.
#[derive(Debug, Clone)]
pub struct DeleteReport {
    /// The workspace dir that was (or would be) removed.
    pub workspace_dir: PathBuf,
    /// Peer-group names whose `agents` list contained the alias and was
    /// (or would be) rewritten to drop it.
    pub peer_group_memberships: Vec<String>,
    /// `true` when `DeleteOptions::dry_run` was set; nothing was touched.
    pub dry_run: bool,
    /// In-flight session count observed at report time. On the
    /// destructive path this is the value the active-session gate
    /// compared against; on the dry-run path it lets an operator see
    /// whether a real delete would be refused without coupling the
    /// inspection to the gate.
    pub active_sessions: usize,
}

/// One row of [`list_agents`]'s output.
#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub alias: String,
    pub risk_profile: String,
    pub model_provider: String,
    pub memory_backend: MemoryBackendKind,
    pub channels: Vec<String>,
}

/// Create a new agent: write the config block, create the workspace
/// dir, seed bootstrap identity files. Atomic at the config-save
/// layer (the canonical temp+fsync+rename pattern). Returns the
/// resolved workspace path so callers can log or chain follow-up
/// setup against it.
///
/// Refuses when:
/// - the alias is already configured (no overwrite);
/// - the risk profile is not a configured `[risk_profiles.<name>]`
///   entry.
pub async fn create_agent(config: &mut Config, spec: AgentSpec) -> Result<PathBuf> {
    if config.agents.contains_key(&spec.alias) {
        bail!(
            "agent {alias:?} already exists; refusing to overwrite",
            alias = spec.alias
        );
    }
    if !config.risk_profiles.contains_key(&spec.risk_profile) {
        bail!(
            "risk_profile {profile:?} is not configured; create it under \
             [risk_profiles.<alias>] before binding it to an agent",
            profile = spec.risk_profile,
        );
    }

    let mut agent = AliasedAgentConfig {
        risk_profile: spec.risk_profile.clone(),
        ..AliasedAgentConfig::default()
    };
    if let Some(provider) = spec.model_provider {
        agent.model_provider = provider.into();
    }
    if let Some(backend) = spec.memory_backend {
        agent.memory.backend = backend;
    }

    let workspace_dir = config.agent_workspace_dir(&spec.alias);
    tokio::fs::create_dir_all(&workspace_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create per-agent workspace at {}",
                workspace_dir.display()
            )
        })?;
    ensure_bootstrap_files(&workspace_dir)
        .await
        .with_context(|| {
            format!(
                "failed to seed bootstrap identity files in {}",
                workspace_dir.display()
            )
        })?;

    config.agents.insert(spec.alias.clone(), agent);
    config.save().await.context("failed to save config")?;

    Ok(workspace_dir)
}

/// Delete an agent: drop the `[agents.<alias>]` block, strip the
/// alias from every `[peer_groups.<name>].agents` list, save the
/// config, then remove the workspace dir. The save happens before
/// the dir removal so a crash mid-delete leaves a config that the
/// schema validator can still load (no dangling peer-group refs);
/// the orphaned workspace dir is recoverable manually.
///
/// Refuses when:
/// - the alias is not configured;
/// - active sessions exist for the alias (unless
///   `opts.force_active_sessions` is `true`).
///
/// `opts.dry_run = true` returns the impact report without touching
/// any on-disk state.
pub async fn delete_agent(
    config: &mut Config,
    alias: &str,
    opts: DeleteOptions,
) -> Result<DeleteReport> {
    if !config.agents.contains_key(alias) {
        bail!("agent {alias:?} is not configured");
    }

    let workspace_dir = config.agent_workspace_dir(alias);
    let peer_group_memberships: Vec<String> = config
        .peer_groups
        .iter()
        .filter(|(_, group)| group.agents.iter().any(|a| a.as_str() == alias))
        .map(|(name, _)| name.clone())
        .collect();

    let active = session_registry::active_sessions_for(alias);

    if opts.dry_run {
        // Dry-run is a read-only impact report. Surface the
        // active-session count so the operator can see whether a
        // real delete would be refused without coupling the
        // inspection to the gate; the gate itself only fires on the
        // destructive path below.
        return Ok(DeleteReport {
            workspace_dir,
            peer_group_memberships,
            dry_run: true,
            active_sessions: active,
        });
    }

    if active > 0 && !opts.force_active_sessions {
        bail!(
            "agent {alias:?} has {active} active session(s); refuse to delete. \
             Stop the running sessions first, or pass force_active_sessions=true \
             to override (the in-flight agent loop will surface I/O errors when \
             its workspace disappears)."
        );
    }

    if active > 0 && opts.force_active_sessions {
        // Loud audit trail for the force path. A scripted operator
        // running force_active_sessions = true otherwise leaves no
        // record of which agent's in-flight sessions were ripped out
        // from under the running loop.
        tracing::warn!(
            agent_alias = alias,
            active_sessions = active,
            "delete_agent: forcing through {active} active session(s) (force_active_sessions=true)"
        );
    }

    config.agents.remove(alias);
    for group_name in &peer_group_memberships {
        if let Some(group) = config.peer_groups.get_mut(group_name) {
            group.agents.retain(|a| a.as_str() != alias);
        }
    }
    config.save().await.context("failed to save config")?;

    if workspace_dir.exists() {
        tokio::fs::remove_dir_all(&workspace_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to remove agent workspace at {}",
                    workspace_dir.display()
                )
            })?;
    }

    Ok(DeleteReport {
        workspace_dir,
        peer_group_memberships,
        dry_run: false,
        active_sessions: active,
    })
}

/// Return one [`AgentSummary`] per configured agent, sorted by alias.
#[must_use]
pub fn list_agents(config: &Config) -> Vec<AgentSummary> {
    let mut aliases: Vec<&String> = config.agents.keys().collect();
    aliases.sort();
    aliases
        .into_iter()
        .map(|alias| {
            let cfg = &config.agents[alias];
            AgentSummary {
                alias: alias.clone(),
                risk_profile: cfg.risk_profile.clone(),
                model_provider: cfg.model_provider.as_str().to_string(),
                memory_backend: cfg.memory.backend,
                channels: cfg
                    .channels
                    .iter()
                    .map(|c| c.as_str().to_string())
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_config::multi_agent::{AgentAlias, PeerGroupConfig};
    use zeroclaw_config::providers::ChannelRef;
    use zeroclaw_config::schema::RiskProfileConfig;

    fn config_in_tempdir(tmp: &TempDir) -> Config {
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config
            .risk_profiles
            .insert("default".into(), RiskProfileConfig::default());
        config
    }

    fn make_spec(alias: &str) -> AgentSpec {
        AgentSpec {
            alias: alias.into(),
            risk_profile: "default".into(),
            model_provider: None,
            memory_backend: None,
        }
    }

    // The session registry is process-global; each test uses a unique
    // alias prefix so parallel runs don't see each other's guards.

    #[tokio::test]
    async fn create_agent_writes_config_block_and_workspace_dir() {
        let alias = "agents_test_create_basic";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);

        let workspace_dir = create_agent(&mut config, make_spec(alias))
            .await
            .expect("create_agent succeeds");

        assert!(workspace_dir.exists(), "workspace dir must be created");
        assert!(
            workspace_dir.join("IDENTITY.md").exists() && workspace_dir.join("SOUL.md").exists(),
            "bootstrap identity files must be seeded"
        );
        assert!(
            config.agents.contains_key(alias),
            "config must contain the new agent's block"
        );
        assert_eq!(config.agents[alias].risk_profile, "default");
    }

    #[tokio::test]
    async fn create_agent_refuses_duplicate_alias() {
        let alias = "agents_test_create_dup";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        create_agent(&mut config, make_spec(alias)).await.unwrap();

        let err = create_agent(&mut config, make_spec(alias))
            .await
            .expect_err("second create with same alias must fail");
        assert!(
            err.to_string().contains("already exists"),
            "expected duplicate-alias error, got: {err}"
        );
    }

    #[tokio::test]
    async fn create_agent_refuses_unknown_risk_profile() {
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        let spec = AgentSpec {
            risk_profile: "missing".into(),
            ..make_spec("agents_test_create_unknown_profile")
        };

        let err = create_agent(&mut config, spec)
            .await
            .expect_err("unknown risk_profile must be rejected");
        assert!(
            err.to_string().contains("not configured"),
            "expected unknown-risk-profile error, got: {err}"
        );
    }

    #[tokio::test]
    async fn delete_agent_removes_workspace_and_strips_peer_group_membership() {
        let alias_a = "agents_test_delete_basic_a";
        let alias_b = "agents_test_delete_basic_b";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        let a_dir = create_agent(&mut config, make_spec(alias_a)).await.unwrap();
        let _ = create_agent(&mut config, make_spec(alias_b)).await.unwrap();

        config.peer_groups.insert(
            "research".into(),
            PeerGroupConfig {
                channel: ChannelRef::from("telegram.prod"),
                agents: vec![AgentAlias::from(alias_a), AgentAlias::from(alias_b)],
                external_peers: vec![],
                ignore: vec![],
            },
        );

        let report = delete_agent(&mut config, alias_a, DeleteOptions::default())
            .await
            .expect("delete succeeds");

        assert!(!report.dry_run);
        assert_eq!(report.workspace_dir, a_dir);
        assert_eq!(report.peer_group_memberships, vec!["research".to_string()]);
        assert!(!a_dir.exists(), "workspace dir must be removed");
        assert!(!config.agents.contains_key(alias_a));
        assert_eq!(
            config.peer_groups["research"].agents,
            vec![AgentAlias::from(alias_b)],
            "alias must be stripped from peer-group memberships"
        );
    }

    #[tokio::test]
    async fn delete_agent_dry_run_changes_nothing_on_disk() {
        let alias = "agents_test_delete_dry_run";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        let dir = create_agent(&mut config, make_spec(alias)).await.unwrap();

        let report = delete_agent(
            &mut config,
            alias,
            DeleteOptions {
                dry_run: true,
                ..DeleteOptions::default()
            },
        )
        .await
        .expect("dry-run delete reports impact without applying");

        assert!(report.dry_run);
        assert_eq!(report.workspace_dir, dir);
        assert!(dir.exists(), "dry_run must NOT remove the workspace");
        assert!(
            config.agents.contains_key(alias),
            "dry_run must NOT drop the config block"
        );
    }

    #[tokio::test]
    async fn delete_agent_refuses_unknown_alias() {
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);

        let err = delete_agent(
            &mut config,
            "agents_test_delete_ghost",
            DeleteOptions::default(),
        )
        .await
        .expect_err("delete of unknown alias must fail");
        assert!(
            err.to_string().contains("not configured"),
            "expected not-configured error, got: {err}"
        );
    }

    #[tokio::test]
    async fn delete_agent_refuses_when_active_sessions_present() {
        let alias = "agents_test_delete_refuses_active";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        create_agent(&mut config, make_spec(alias)).await.unwrap();

        // Hold a session guard for the alias; delete must refuse while
        // it's in scope, then succeed once the guard drops.
        {
            let _guard = session_registry::register_session(alias);
            assert_eq!(session_registry::active_sessions_for(alias), 1);

            let err = delete_agent(&mut config, alias, DeleteOptions::default())
                .await
                .expect_err("delete must refuse on active session");
            assert!(
                err.to_string().contains("active session"),
                "expected active-session error, got: {err}"
            );
        }

        assert_eq!(session_registry::active_sessions_for(alias), 0);
        delete_agent(&mut config, alias, DeleteOptions::default())
            .await
            .expect("delete succeeds once sessions clear");
    }

    #[tokio::test]
    async fn delete_agent_dry_run_reports_active_session_count() {
        let alias = "agents_test_delete_dry_run_reports_active";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        create_agent(&mut config, make_spec(alias)).await.unwrap();

        let _guard = session_registry::register_session(alias);

        let report = delete_agent(
            &mut config,
            alias,
            DeleteOptions {
                dry_run: true,
                ..DeleteOptions::default()
            },
        )
        .await
        .expect("dry_run inspection must succeed even with active sessions");

        assert!(report.dry_run);
        assert_eq!(
            report.active_sessions, 1,
            "dry_run must surface the active-session count an operator would otherwise have to inspect separately"
        );
    }

    #[tokio::test]
    async fn delete_agent_force_active_sessions_overrides_refusal() {
        let alias = "agents_test_delete_force";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        create_agent(&mut config, make_spec(alias)).await.unwrap();

        let _guard = session_registry::register_session(alias);
        delete_agent(
            &mut config,
            alias,
            DeleteOptions {
                force_active_sessions: true,
                ..DeleteOptions::default()
            },
        )
        .await
        .expect("force_active_sessions must override the refusal");
        assert!(!config.agents.contains_key(alias));
    }

    #[tokio::test]
    async fn list_agents_returns_sorted_summaries() {
        let alias_first = "agents_test_list_aaa";
        let alias_second = "agents_test_list_zzz";
        let tmp = TempDir::new().unwrap();
        let mut config = config_in_tempdir(&tmp);
        create_agent(&mut config, make_spec(alias_second))
            .await
            .unwrap();
        create_agent(&mut config, make_spec(alias_first))
            .await
            .unwrap();

        let summaries = list_agents(&config);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].alias, alias_first);
        assert_eq!(summaries[1].alias, alias_second);
        assert_eq!(summaries[0].risk_profile, "default");
        assert_eq!(summaries[0].memory_backend, MemoryBackendKind::Sqlite);
    }
}
