//! Integration catalog — schema-driven, single-loop.
//!
//! Every entry comes from a schema-side source:
//! - Channels: `ChannelsConfig::channels()` (each multi-instance V3
//!   channel field surfaces as one entry; `ChannelConfig::name()` and
//!   `ChannelConfig::desc()` are the source of display text).
//! - Toggle integrations: `Config::integration_descriptors()` (per-struct
//!   `#[integration(...)]` attribute on `BrowserConfig` / `CronConfig` /
//!   `GoogleWorkspaceConfig`). V3 currently returns an empty list here
//!   because those structs have not yet been ported to the
//!   `#[integration(...)]` schema annotation; tracked for the
//!   v0.7.5 → v0.8.0 follow-on or PR #6403's conflict-resolution pass.
//! - AI providers: `zeroclaw_providers::list_providers()` (each
//!   `ProviderInfo` row carries `display_name`, `description`, and a
//!   `ProviderActivation` strategy).
//! - Always-on built-in tools: `crate::tools::BUILTIN_TOOL_INTEGRATIONS`.
//! - Platforms: `super::platform::PLATFORMS` (compile-time `cfg!` facts).
//!
//! No string literal naming a channel, vendor, tool, or platform appears
//! in this file's production path. Adding a new integration of any kind
//! is one row in the corresponding schema source — the registry picks
//! it up automatically.

use super::platform::PLATFORMS;
use super::{IntegrationCategory, IntegrationEntry, IntegrationStatus};
use crate::tools::BUILTIN_TOOL_INTEGRATIONS;
use zeroclaw_config::schema::Config;
use zeroclaw_providers::ProviderActivation;

fn bool_to_status(active: bool) -> IntegrationStatus {
    if active {
        IntegrationStatus::Active
    } else {
        IntegrationStatus::Available
    }
}

/// Map the schema-side `#[integration(category = "...")]` label to the
/// runtime enum. The schema crate intentionally keeps the label as a
/// string to avoid taking a dependency on this crate's enum.
fn parse_category(label: &str) -> IntegrationCategory {
    match label {
        "Chat" => IntegrationCategory::Chat,
        "AiModel" => IntegrationCategory::AiModel,
        "ToolsAutomation" => IntegrationCategory::ToolsAutomation,
        "Platform" => IntegrationCategory::Platform,
        // Defensive default; the schema's `#[integration(category = ...)]`
        // attribute is the source of truth for valid labels.
        _ => IntegrationCategory::ToolsAutomation,
    }
}

/// Compute an AI-model integration's status from its `ProviderActivation`
/// strategy. The registry never branches on a provider name — every
/// per-vendor decision lives on the `ProviderInfo` row in
/// `zeroclaw_providers::list_providers()`.
///
/// V3 has no global `providers.fallback`; activation is derived from
/// the presence of `[providers.models.<type>.<alias>]` entries that
/// match the provider info's name or aliases. Strategy variants that
/// previously consulted the global fallback now consult the same
/// per-type matching logic plus their original side condition.
fn evaluate_provider_activation(
    config: &Config,
    info: &zeroclaw_providers::ProviderInfo,
) -> IntegrationStatus {
    let provider_type_matches =
        |type_key: &str| -> bool { type_key == info.name || info.aliases.contains(&type_key) };

    let active = match info.activation {
        ProviderActivation::FallbackKey => config
            .providers
            .models
            .keys()
            .any(|k| provider_type_matches(k.as_str())),
        ProviderActivation::FallbackKeyWithApiKey => config
            .providers
            .models
            .iter()
            .filter(|(k, _)| provider_type_matches(k.as_str()))
            .any(|(_, aliases)| aliases.values().any(|p| p.api_key.is_some())),
        ProviderActivation::ModelPrefix(prefix) => config
            .providers
            .models
            .values()
            .flat_map(|aliases| aliases.values())
            .any(|p| p.model.as_deref().is_some_and(|m| m.starts_with(prefix))),
        ProviderActivation::FallbackKeyMatches(predicate) => config
            .providers
            .models
            .keys()
            .any(|k| predicate(k.as_str())),
    };
    bool_to_status(active)
}

/// Returns the integration catalog computed against `config`.
///
/// Single-loop, schema-driven. Every per-row decision lives on the
/// schema-side source; this function just concatenates the iterators.
///
/// Channel discovery walks `ChannelsConfig::channels()` so each
/// channel type's `ChannelConfig::name()` and `ChannelConfig::desc()`
/// are the single source of display text — no string literal naming
/// a channel appears in this file's production path. Multi-instance
/// V3 channels are reported active when any alias is configured.
pub fn all_integrations(config: &Config) -> Vec<IntegrationEntry> {
    let channels = config
        .channels
        .channels()
        .into_iter()
        .map(|(handle, active)| IntegrationEntry {
            name: handle.name().to_string(),
            description: handle.desc().to_string(),
            category: IntegrationCategory::Chat,
            status: bool_to_status(active),
        });

    let toggles = config
        .integration_descriptors()
        .into_iter()
        .map(|d| IntegrationEntry {
            name: d.display_name.to_string(),
            description: d.description.to_string(),
            category: parse_category(d.category),
            status: bool_to_status(d.active),
        });

    let providers = zeroclaw_providers::list_providers()
        .into_iter()
        .map(|info| {
            let status = evaluate_provider_activation(config, &info);
            IntegrationEntry {
                name: info.display_name.to_string(),
                description: info.description.to_string(),
                category: IntegrationCategory::AiModel,
                status,
            }
        });

    let builtins = BUILTIN_TOOL_INTEGRATIONS
        .iter()
        .map(|(name, desc)| IntegrationEntry {
            name: (*name).to_string(),
            description: (*desc).to_string(),
            category: IntegrationCategory::ToolsAutomation,
            status: IntegrationStatus::Active,
        });

    let platforms = PLATFORMS.iter().map(|(name, available)| IntegrationEntry {
        name: (*name).to_string(),
        description: String::new(),
        category: IntegrationCategory::Platform,
        status: bool_to_status(*available),
    });

    channels
        .chain(toggles)
        .chain(providers)
        .chain(builtins)
        .chain(platforms)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::Config;
    use zeroclaw_config::schema::{IMessageConfig, MatrixConfig, StreamMode, TelegramConfig};
    use zeroclaw_config::traits::ChannelConfig;

    #[test]
    fn registry_has_entries() {
        let config = Config::default();
        let entries = all_integrations(&config);
        assert!(
            entries.len() >= 30,
            "Expected 30+ integrations, got {}",
            entries.len()
        );
    }

    #[test]
    fn all_categories_represented() {
        let config = Config::default();
        let entries = all_integrations(&config);
        for cat in IntegrationCategory::all() {
            let count = entries.iter().filter(|e| e.category == *cat).count();
            assert!(count > 0, "Category {cat:?} has no entries");
        }
    }

    #[test]
    fn no_duplicate_names() {
        let config = Config::default();
        let entries = all_integrations(&config);
        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                seen.insert(entry.name.clone()),
                "Duplicate integration name: {}",
                entry.name
            );
        }
    }

    #[test]
    fn channel_entries_carry_per_field_metadata_from_schema() {
        // Schema-driven contract: every Option<XConfig> on ChannelsConfig
        // surfaces as a Chat entry whose display_name and description
        // come from the field's `#[display_name = ...]` /
        // `#[description = ...]` attributes — no override table here.
        let config = Config::default();
        let entries = all_integrations(&config);
        let channel_count = entries
            .iter()
            .filter(|e| e.category == IntegrationCategory::Chat)
            .count();
        let nested_count = config.channels.nested_option_entries().len();
        assert_eq!(
            channel_count, nested_count,
            "every Option<XConfig> field should produce exactly one Chat entry",
        );
        for nested in config.channels.nested_option_entries() {
            let entry = entries
                .iter()
                .find(|e| e.name == nested.display_name)
                .unwrap_or_else(|| {
                    panic!(
                        "channel field {:?} (display {:?}) missing from registry",
                        nested.field, nested.display_name,
                    )
                });
            assert!(
                !entry.name.is_empty(),
                "channel field {:?} produced empty display name",
                nested.field
            );
            assert!(
                !entry.description.is_empty(),
                "channel field {:?} (display {:?}) missing #[description = ...] attribute",
                nested.field,
                nested.display_name,
            );
        }
    }

    #[test]
    fn telegram_active_when_configured() {
        let mut config = Config::default();
        config.channels.telegram.insert(
            "default".to_string(),
            TelegramConfig {
                bot_token: "123:ABC".into(),
                allowed_users: vec!["user".into()],
                stream_mode: StreamMode::default(),
                draft_update_interval_ms: 1000,
                interrupt_on_new_message: false,
                mention_only: false,
                ack_reactions: None,
                proxy_url: None,
                approval_timeout_secs: 120,
                excluded_tools: vec![],
            },
        );
        let entries = all_integrations(&config);
        let display_name = <TelegramConfig as ChannelConfig>::name();
        let tg = entries.iter().find(|e| e.name == display_name).unwrap();
        assert!(matches!(tg.status, IntegrationStatus::Active));
    }

    #[test]
    fn telegram_available_when_not_configured() {
        let config = Config::default();
        let entries = all_integrations(&config);
        let nested = config
            .channels
            .nested_option_entries()
            .into_iter()
            .find(|e| e.field == "telegram")
            .expect("telegram field declared on ChannelsConfig");
        let tg = entries
            .iter()
            .find(|e| e.name == nested.display_name)
            .unwrap();
        assert!(matches!(tg.status, IntegrationStatus::Available));
    }

    #[test]
    fn imessage_active_when_configured() {
        let mut config = Config::default();
        config.channels.imessage.insert(
            "default".to_string(),
            IMessageConfig {
                allowed_contacts: vec!["*".into()],
                excluded_tools: vec![],
            },
        );
        let entries = all_integrations(&config);
        let display_name = <IMessageConfig as ChannelConfig>::name();
        let im = entries.iter().find(|e| e.name == display_name).unwrap();
        assert!(matches!(im.status, IntegrationStatus::Active));
    }

    #[test]
    fn imessage_available_when_not_configured() {
        let config = Config::default();
        let entries = all_integrations(&config);
        let display_name = <IMessageConfig as ChannelConfig>::name();
        let im = entries.iter().find(|e| e.name == display_name).unwrap();
        assert!(matches!(im.status, IntegrationStatus::Available));
    }

    #[test]
    fn matrix_active_when_configured() {
        let mut config = Config::default();
        config.channels.matrix.insert(
            "default".to_string(),
            MatrixConfig {
                homeserver: "https://m.org".into(),
                access_token: Some("tok".into()),
                user_id: None,
                device_id: None,
                allowed_users: vec![],
                allowed_rooms: vec!["!r:m".into()],
                interrupt_on_new_message: false,
                stream_mode: zeroclaw_config::schema::StreamMode::default(),
                draft_update_interval_ms: 1500,
                multi_message_delay_ms: 800,
                recovery_key: None,
                password: None,
                mention_only: false,
                approval_timeout_secs: 300,
                reply_in_thread: true,
                ack_reactions: true,
                excluded_tools: vec![],
            },
        );
        let entries = all_integrations(&config);
        let display_name = <MatrixConfig as ChannelConfig>::name();
        let mx = entries.iter().find(|e| e.name == display_name).unwrap();
        assert!(matches!(mx.status, IntegrationStatus::Active));
    }

    // V3 doesn't yet annotate BrowserConfig / CronConfig /
    // GoogleWorkspaceConfig with `#[integration(...)]`, so
    // `Config::integration_descriptors()` returns empty and the
    // `toggles` chain in the registry produces no entries today.
    // The cron / browser / google-workspace integration_descriptor
    // tests that landed on master are intentionally omitted until
    // those schema attributes are ported forward (tracked as a
    // follow-up to the v0.7.5 → v0.8.0 merge).

    #[test]
    fn builtin_tool_integrations_always_active() {
        // Drift detector: every row in BUILTIN_TOOL_INTEGRATIONS must
        // surface as an Active entry. Adding / removing a built-in is
        // the single edit point.
        let config = Config::default();
        let entries = all_integrations(&config);
        for (name, _desc) in BUILTIN_TOOL_INTEGRATIONS {
            let entry = entries
                .iter()
                .find(|e| e.name == *name)
                .unwrap_or_else(|| panic!("built-in {name:?} missing from registry"));
            assert!(
                matches!(entry.status, IntegrationStatus::Active),
                "{name} should always be Active",
            );
        }
    }

    #[test]
    fn platforms_match_compile_time_constants() {
        let config = Config::default();
        let entries = all_integrations(&config);
        for (name, available) in PLATFORMS {
            let entry = entries
                .iter()
                .find(|e| e.name == *name)
                .unwrap_or_else(|| panic!("platform {name:?} missing from registry"));
            let expected = bool_to_status(*available);
            assert_eq!(
                entry.status, expected,
                "platform {name:?} status disagrees with PLATFORMS const",
            );
        }
    }

    #[test]
    fn regional_provider_aliases_activate_expected_ai_integrations() {
        // For each multi-region family that uses
        // `ProviderActivation::FallbackKeyMatches`, configuring a
        // `[providers.models.<provider_type>.<alias>]` entry must mark
        // the corresponding `ProviderInfo`-derived integration entry as
        // Active. Looks the entry up by canonical name (not
        // display_name) so display copy can change without breaking
        // the contract. V3 has no global `providers.fallback`; the
        // FallbackKeyMatches predicate is run against the keys of
        // `providers.models` instead.
        let cases = [
            ("minimax-cn", "minimax"),
            ("glm-cn", "glm"),
            ("moonshot-intl", "moonshot"),
            ("qwen-intl", "qwen"),
            ("zai-cn", "zai"),
            ("baidu", "qianfan"),
        ];
        for (provider_type, canonical) in cases {
            let mut config = Config::default();
            config
                .providers
                .models
                .entry(provider_type.to_string())
                .or_default()
                .entry("default".to_string())
                .or_default();
            let entries = all_integrations(&config);
            let info = zeroclaw_providers::list_providers()
                .into_iter()
                .find(|p| p.name == canonical)
                .unwrap_or_else(|| {
                    panic!("ProviderInfo for canonical name {canonical:?} must exist")
                });
            let integration = entries
                .iter()
                .find(|e| e.name == info.display_name)
                .unwrap_or_else(|| {
                    panic!(
                        "integration entry for {canonical:?} (display {:?}) must exist",
                        info.display_name,
                    )
                });
            assert!(
                matches!(integration.status, IntegrationStatus::Active),
                "provider type {provider_type:?} must activate {canonical:?} integration",
            );
        }
    }
}
