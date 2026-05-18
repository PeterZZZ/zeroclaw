//! CLI handler for the `zeroclaw dream` command.

use anyhow::{Context, Result};
use zeroclaw_config::schema::Config;
use zeroclaw_runtime::dream::pending::DreamPending;
use zeroclaw_runtime::dream::report::DreamReport;
use zeroclaw_runtime::i18n::{get_cli_string_with_args, get_required_cli_string};

/// Run a manual dream cycle from the CLI.
pub async fn run_dream(config: &Config, dry_run: bool, verbose: bool) -> Result<()> {
    use zeroclaw_runtime::dream::engine::DreamEngine;

    // Dry-run is enforced via run_cycle_with_options below — audit_mode is left
    // as-configured so dry-run doesn't silently flip behaviour in the report.
    let engine = DreamEngine::new(config.dream_mode.clone(), config.workspace_dir.clone());

    // Resolve provider only if dream_mode.model is configured (opt-in LLM).
    let (provider, model): (
        Option<Box<dyn zeroclaw_api::provider::Provider>>,
        Option<String>,
    ) = if config.dream_mode.model.is_some() {
        let fallback = config
            .providers
            .fallback_provider()
            .context("dream: dream_mode.model set but no fallback provider configured")?;
        let provider_name = config.providers.fallback.as_deref().unwrap_or("anthropic");
        let model_name = config
            .dream_mode
            .model
            .as_deref()
            .or(fallback.model.as_deref())
            .unwrap_or("claude-haiku-4-5-20251001")
            .to_string();

        let provider_runtime_options =
            zeroclaw_providers::provider_runtime_options_from_config(config);
        let p = zeroclaw_providers::create_routed_provider_with_options(
            provider_name,
            fallback.api_key.as_deref(),
            fallback.base_url.as_deref(),
            &config.reliability,
            &config.providers.model_routes,
            &model_name,
            &provider_runtime_options,
        )?;
        (Some(p), Some(model_name))
    } else {
        (None, None)
    };

    // Create memory backend.
    let memory = zeroclaw_memory::create_memory(
        &config.memory,
        &config.workspace_dir,
        config
            .providers
            .fallback_provider()
            .and_then(|e| e.api_key.as_deref()),
    )
    .context("dream: failed to create memory backend")?;

    if verbose {
        let mode_str = if model.is_some() {
            "LLM-assisted"
        } else {
            "local-only"
        };
        let model_display = model.as_deref().unwrap_or("(none)");
        println!(
            "{}",
            get_cli_string_with_args(
                "cli-dream-starting",
                &[
                    ("provider", mode_str),
                    ("model", model_display),
                    ("backend", memory.name()),
                ],
            )
            .unwrap_or_else(|| format!(
                "Dream cycle starting...\n  Mode: {mode_str}\n  Model: {model_display}\n  Memory backend: {}",
                memory.name()
            ))
        );
        if dry_run {
            println!("{}", get_required_cli_string("cli-dream-dry-run-mode"));
        }
    }

    let result = engine
        .run_cycle_with_options(
            memory.as_ref(),
            provider.as_ref().map(|p| p.as_ref()),
            model.as_deref(),
            dry_run,
        )
        .await?;

    println!(
        "{}",
        get_cli_string_with_args(
            "cli-dream-complete",
            &[
                ("gathered", &result.gathered_count.to_string()),
                ("consolidated", &result.consolidated_count.to_string()),
                ("pruned", &result.pruned_count.to_string()),
            ],
        )
        .unwrap_or_else(|| format!(
            "Dream cycle complete: {} memories gathered, {} insights consolidated, {} pruned",
            result.gathered_count, result.consolidated_count, result.pruned_count
        ))
    );

    if !result.insights.is_empty() {
        println!("\n{}", get_required_cli_string("cli-dream-insights-header"));
        for (i, insight) in result.insights.iter().enumerate() {
            println!("  {}. {insight}", i + 1);
        }
    }

    if let Some(ref summary) = result.report_summary {
        println!(
            "\n{}",
            get_cli_string_with_args("cli-dream-summary", &[("summary", summary.as_str())])
                .unwrap_or_else(|| format!("Summary: {summary}"))
        );
    }

    if dry_run {
        println!("\n{}", get_required_cli_string("cli-dream-dry-run-notice"));
    } else if config.dream_mode.audit_mode {
        println!("\n{}", get_required_cli_string("cli-dream-staged-notice"));
    }

    Ok(())
}

/// Show the pending dream report, if any.
pub fn show_report(config: &Config) -> Result<()> {
    match DreamReport::load_pending(&config.workspace_dir)? {
        Some(report) => {
            println!("{}", report.format_message());
            DreamReport::mark_delivered(&config.workspace_dir)?;
        }
        None => {
            println!("{}", get_required_cli_string("cli-dream-no-report"));
        }
    }
    Ok(())
}

/// Promote staged dream mutations from `dream_pending.json` into memory.
pub async fn promote(config: &Config) -> Result<()> {
    use zeroclaw_api::memory_traits::{Memory, MemoryCategory};

    let pending = DreamPending::load(&config.workspace_dir)?;
    let Some(pending) = pending else {
        println!("{}", get_required_cli_string("cli-dream-no-pending"));
        return Ok(());
    };

    println!(
        "{}",
        get_cli_string_with_args(
            "cli-dream-promote-summary",
            &[
                ("insights", &pending.insights.len().to_string()),
                ("prunes", &pending.proposed_prunes.len().to_string()),
            ],
        )
        .unwrap_or_else(|| format!(
            "Promoting {} insights, pruning {} stale keys...",
            pending.insights.len(),
            pending.proposed_prunes.len()
        ))
    );

    let memory = zeroclaw_memory::create_memory(
        &config.memory,
        &config.workspace_dir,
        config
            .providers
            .fallback_provider()
            .and_then(|e| e.api_key.as_deref()),
    )
    .context("dream promote: failed to create memory backend")?;

    let mut stored = 0usize;
    for insight in &pending.insights {
        let key = format!("dream_insight_{}", uuid::Uuid::new_v4());
        match memory
            .store_with_metadata(
                &key,
                &insight.content,
                MemoryCategory::Core,
                None,
                Some("dream"),
                Some(insight.importance),
            )
            .await
        {
            Ok(()) => stored += 1,
            Err(e) => {
                let err_str = e.to_string();
                eprintln!(
                    "{}",
                    get_cli_string_with_args(
                        "cli-dream-promote-store-error",
                        &[("error", err_str.as_str())],
                    )
                    .unwrap_or_else(|| format!("  Failed to store insight: {err_str}"))
                );
            }
        }
    }

    let mut pruned = 0usize;
    for key in &pending.proposed_prunes {
        match memory.forget(key).await {
            Ok(true) => pruned += 1,
            Ok(false) => {}
            Err(e) => {
                let err_str = e.to_string();
                eprintln!(
                    "{}",
                    get_cli_string_with_args(
                        "cli-dream-promote-prune-error",
                        &[("key", key.as_str()), ("error", err_str.as_str())],
                    )
                    .unwrap_or_else(|| format!("  Failed to prune key {key}: {err_str}"))
                );
            }
        }
    }

    DreamPending::clear(&config.workspace_dir)?;

    println!(
        "{}",
        get_cli_string_with_args(
            "cli-dream-promote-done",
            &[
                ("stored", &stored.to_string()),
                ("pruned", &pruned.to_string()),
            ],
        )
        .unwrap_or_else(|| format!("Done: {stored} insights stored, {pruned} memories pruned."))
    );

    Ok(())
}
