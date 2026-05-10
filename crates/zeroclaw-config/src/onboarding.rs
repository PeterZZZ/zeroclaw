//! Canonical onboarding-wizard section list — the single source of truth
//! for which top-level config sections appear in the `/onboard` flow and in
//! what order. Every consumer (CLI runtime, TUI, gateway dashboard) reads
//! from this const so the three surfaces never drift apart.
//!
//! Adding or reordering an onboarding section means editing this list and
//! nothing else. Sections that exist on `Config` but are NOT here (gateway,
//! observability, scheduler, …) live exclusively in the post-setup
//! `/config` explorer; the `/onboard` wizard is intentionally narrower.

/// Onboarding-wizard sections in canonical setup order. The order encodes
/// dependencies: structural sections come first so later sections can
/// reference what's been configured (channels need providers, agents need
/// channels + providers + risk profiles, …).
///
/// `agents` is last by RFC #5890 — composing the rest of the system into
/// a working agent is the final step, never a prerequisite.
pub const ONBOARDING_WIZARD_SECTIONS: &[&str] = &[
    "workspace",
    "model_providers",
    "tts_providers",
    "transcription_providers",
    "channels",
    "memory",
    "hardware",
    "tunnel",
    "personality",
    "agents",
];

/// Index of `key` in [`ONBOARDING_WIZARD_SECTIONS`], or `None` if it's not
/// part of the onboarding wizard. Used to sort gateway-discovered sections
/// into canonical order without duplicating the list.
#[must_use]
pub fn onboarding_section_index(key: &str) -> Option<usize> {
    ONBOARDING_WIZARD_SECTIONS.iter().position(|s| *s == key)
}

/// True when `key` is part of the `/onboard` wizard (vs. a `/config`-only
/// section like `gateway` or `observability`).
#[must_use]
pub fn is_onboarding_section(key: &str) -> bool {
    onboarding_section_index(key).is_some()
}
