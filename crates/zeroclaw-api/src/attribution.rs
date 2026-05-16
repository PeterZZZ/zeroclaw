//! Alias-bound attribution surface used by every emission in the
//! workspace. Each "thing" that participates in an event (channel,
//! agent, tool, cron job, model provider, memory backend, peer group,
//! skill bundle, MCP bundle, session) implements [`Attributable`].
//! Entry points open `attribution_span()` once at the start of their
//! work; the `LogCaptureLayer` in `zeroclaw-log` walks the span scope
//! and fills the typed attribution slots automatically.
//!
//! The trait does not own a `Span` constructor here — that lives in
//! `zeroclaw-log::attribution_span` to keep the tracing dependency
//! contained. Each `Attributable` only exposes its role + alias; the
//! emission crate turns those into the right span shape.
//!
//! Adding a new variant: extend the relevant `Kind` enum and add the
//! mapping in the [`Role::composite_prefix`] / [`Role::composite_type`]
//! / [`Role::attribution_field`] methods. No call-site changes.

/// Trait every alias-bound "thing" implements once next to its struct.
///
/// The two methods are the contract. The default span construction
/// happens in `zeroclaw-log::attribution_span(thing)` and uses these
/// two to populate the typed slots — no per-impl span code needed.
pub trait Attributable {
    /// The role this thing fills (Channel/Agent/Tool/Cron/Provider/...).
    fn role(&self) -> Role;

    /// The alias portion of the `<type>.<alias>` composite (or the
    /// plain attribution value for non-composite roles). For an Agent
    /// this is `agent_alias`; for a Telegram channel it is the
    /// `[channels.telegram.<alias>]` config key suffix.
    fn alias(&self) -> &str;
}

/// Closed taxonomy of every role a thing can fill. Adding a new family
/// here is the single point of change for new attribution coverage —
/// the layer reads the typed slots through the methods below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Swarm,
    Agent,
    Channel(ChannelKind),
    Tool(ToolKind),
    Cron(CronKind),
    Provider(ProviderKind),
    Memory(MemoryKind),
    PeerGroup,
    Skill,
    Mcp,
    Session,
    System,
}

/// Channel implementations. The string returned by [`ChannelKind::type_str`]
/// is the canonical `channel_type` value in every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Cli,
    Discord,
    Lark,
    Matrix,
    Slack,
    Telegram,
    Webhook,
    Wechat,
    WhatsappBusiness,
    WhatsappWeb,
}

impl ChannelKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Discord => "discord",
            Self::Lark => "lark",
            Self::Matrix => "matrix",
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::Webhook => "webhook",
            Self::Wechat => "wechat",
            Self::WhatsappBusiness => "whatsapp_business",
            Self::WhatsappWeb => "whatsapp_web",
        }
    }
}

/// Tool implementations. Open-ended in practice (plugins register at
/// runtime) so the catch-all `Other(&'static str)` variant carries the
/// canonical tool name for tools not in the built-in set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Shell,
    HttpRequest,
    HttpServer,
    FetchUrl,
    Search,
    Memory,
    SpawnSubagent,
    SopList,
    SopExecute,
    SopApprove,
    SopAdvance,
    SopStatus,
    SopHistory,
    Wait,
    Other(&'static str),
}

impl ToolKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::HttpRequest => "http_request",
            Self::HttpServer => "http_server",
            Self::FetchUrl => "fetch_url",
            Self::Search => "search",
            Self::Memory => "memory",
            Self::SpawnSubagent => "spawn_subagent",
            Self::SopList => "sop_list",
            Self::SopExecute => "sop_execute",
            Self::SopApprove => "sop_approve",
            Self::SopAdvance => "sop_advance",
            Self::SopStatus => "sop_status",
            Self::SopHistory => "sop_history",
            Self::Wait => "wait",
            Self::Other(name) => name,
        }
    }
}

/// Cron job shapes. Currently a flat schedule taxonomy; nested if a
/// shape acquires its own sub-kinds later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronKind {
    Interval,
    At,
    Cron,
    Once,
}

impl CronKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Interval => "interval",
            Self::At => "at",
            Self::Cron => "cron",
            Self::Once => "once",
        }
    }
}

/// Provider family. The inner enum carries the specific provider
/// implementation; the outer family drives which composite prefix
/// (`model_provider` / `tts_provider` / …) the layer populates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Model(ModelProviderKind),
    Tts(TtsProviderKind),
    Transcription(TranscriptionProviderKind),
    Tunnel(TunnelProviderKind),
}

impl ProviderKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Model(k) => k.type_str(),
            Self::Tts(k) => k.type_str(),
            Self::Transcription(k) => k.type_str(),
            Self::Tunnel(k) => k.type_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProviderKind {
    Anthropic,
    OpenAi,
    Together,
    Bedrock,
    Ollama,
    Gemini,
    GoogleAi,
    Mistral,
    Groq,
    Other(&'static str),
}

impl ModelProviderKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Together => "together",
            Self::Bedrock => "bedrock",
            Self::Ollama => "ollama",
            Self::Gemini => "gemini",
            Self::GoogleAi => "google_ai",
            Self::Mistral => "mistral",
            Self::Groq => "groq",
            Self::Other(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsProviderKind {
    OpenAi,
    ElevenLabs,
    Cartesia,
    Piper,
    Other(&'static str),
}

impl TtsProviderKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::ElevenLabs => "elevenlabs",
            Self::Cartesia => "cartesia",
            Self::Piper => "piper",
            Self::Other(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionProviderKind {
    Whisper,
    OpenAi,
    Deepgram,
    Other(&'static str),
}

impl TranscriptionProviderKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Whisper => "whisper",
            Self::OpenAi => "openai",
            Self::Deepgram => "deepgram",
            Self::Other(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelProviderKind {
    Ngrok,
    Cloudflared,
    OpenVpn,
    Other(&'static str),
}

impl TunnelProviderKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Ngrok => "ngrok",
            Self::Cloudflared => "cloudflared",
            Self::OpenVpn => "openvpn",
            Self::Other(name) => name,
        }
    }
}

/// Memory backend implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Sqlite,
    Json,
    InMemory,
    Other(&'static str),
}

impl MemoryKind {
    #[must_use]
    pub fn type_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Json => "json",
            Self::InMemory => "in_memory",
            Self::Other(name) => name,
        }
    }
}

impl Role {
    /// The composite prefix this role populates (`channel`,
    /// `model_provider`, `tts_provider`, `transcription_provider`,
    /// `tunnel_provider`, `memory_namespace`-derivative), or `None`
    /// for roles that are plain attribution fields.
    #[must_use]
    pub fn composite_prefix(self) -> Option<&'static str> {
        match self {
            Self::Channel(_) => Some("channel"),
            Self::Provider(ProviderKind::Model(_)) => Some("model_provider"),
            Self::Provider(ProviderKind::Tts(_)) => Some("tts_provider"),
            Self::Provider(ProviderKind::Transcription(_)) => Some("transcription_provider"),
            Self::Provider(ProviderKind::Tunnel(_)) => Some("tunnel_provider"),
            Self::Swarm
            | Self::Agent
            | Self::Tool(_)
            | Self::Cron(_)
            | Self::Memory(_)
            | Self::PeerGroup
            | Self::Skill
            | Self::Mcp
            | Self::Session
            | Self::System => None,
        }
    }

    /// The `<type>` portion of the composite, when this role contributes
    /// to one.
    #[must_use]
    pub fn composite_type(self) -> Option<&'static str> {
        match self {
            Self::Channel(k) => Some(k.type_str()),
            Self::Provider(p) => Some(p.type_str()),
            _ => None,
        }
    }

    /// The plain attribution key this role populates for non-composite
    /// roles. `Tool` writes `tool`; `Agent` writes `agent_alias`; `Cron`
    /// writes `cron_job_id`; etc.
    #[must_use]
    pub fn attribution_field(self) -> Option<&'static str> {
        match self {
            Self::Agent => Some("agent_alias"),
            Self::Tool(_) => Some("tool"),
            Self::Cron(_) => Some("cron_job_id"),
            Self::Memory(_) => Some("memory_namespace"),
            Self::PeerGroup => Some("peer_group"),
            Self::Skill => Some("skill_bundle"),
            Self::Mcp => Some("mcp_bundle"),
            Self::Session => Some("session_key"),
            _ => None,
        }
    }

    /// Stable string tag used by the span layer to deserialize the role
    /// back from the span field. One-to-one with the enum variant; the
    /// inner Kind is rendered alongside in [`Role::variant_str`].
    #[must_use]
    pub fn family_str(self) -> &'static str {
        match self {
            Self::Swarm => "swarm",
            Self::Agent => "agent",
            Self::Channel(_) => "channel",
            Self::Tool(_) => "tool",
            Self::Cron(_) => "cron",
            Self::Provider(ProviderKind::Model(_)) => "provider.model",
            Self::Provider(ProviderKind::Tts(_)) => "provider.tts",
            Self::Provider(ProviderKind::Transcription(_)) => "provider.transcription",
            Self::Provider(ProviderKind::Tunnel(_)) => "provider.tunnel",
            Self::Memory(_) => "memory",
            Self::PeerGroup => "peer_group",
            Self::Skill => "skill",
            Self::Mcp => "mcp",
            Self::Session => "session",
            Self::System => "system",
        }
    }

    /// The closest equivalent [`zeroclaw_log::event::EventCategory`] for
    /// this role, used by the layer to default `event.category` when
    /// the call site doesn't override it. Returned as a `&'static str`
    /// to keep `zeroclaw-api` free of a back-dep on `zeroclaw-log`.
    #[must_use]
    pub fn default_category(self) -> &'static str {
        match self {
            Self::Swarm | Self::Agent => "agent",
            Self::Channel(_) => "channel",
            Self::Tool(_) => "tool",
            Self::Cron(_) => "cron",
            Self::Provider(_) => "provider",
            Self::Memory(_) => "memory",
            Self::Session => "session",
            Self::PeerGroup | Self::Skill | Self::Mcp | Self::System => "system",
        }
    }
}
