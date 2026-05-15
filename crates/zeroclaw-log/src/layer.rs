//! `tracing-subscriber` Layer that captures every `tracing::*` event and
//! routes it through the zeroclaw-log pipeline: persisted JSONL +
//! broadcast hook + Observer bridge.
//!
//! Install this layer alongside the existing `fmt::Subscriber` formatter
//! in the daemon's tracing setup. Doing so makes zeroclaw-log THE
//! emission surface for all logging without rewriting 1,300+ call sites.
//! Direct `tracing::info!/warn!/error!/debug!/trace!` calls keep working
//! exactly as they do today (terminal output via the formatter) AND now
//! also land in the JSONL log + the dashboard's SSE stream.
//!
//! High-value sites use the [`crate::record!`] macro for explicit
//! alias-bound attribution. Bare `tracing::*` calls produce log events
//! with whatever structured fields the caller passed; the Layer picks up
//! anything named in [`crate::event::ATTRIBUTION_FIELDS`] or matching a
//! prefix in [`crate::event::COMPOSITE_PREFIXES`].

use std::fmt::Write;

use serde_json::{Map as JsonMap, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::event::{
    ATTRIBUTION_FIELDS, COMPOSITE_PREFIXES, EventCategory, EventOutcome, LogEvent, Severity,
    ZeroclawAttribution,
};
use crate::writer::record_event;

/// Span-extension marker carrying the subsystem's `EventCategory`. The
/// Layer walks the span scope leaf→root when an event doesn't carry an
/// explicit `category` field, so the closest enclosing span wins. The
/// agent loop / channel listener / cron tick top-level spans set this
/// once on entry and every descendant inherits.
#[derive(Clone, Copy)]
struct SpanCategory(EventCategory);

const ACTION_FIELD: &str = "event";
const CATEGORY_FIELD: &str = "category";
const FIELD_OUTCOME: &str = "outcome";
const FIELD_AGENT: &str = "agent";
const FIELD_PARENT_ALIAS: &str = "parent_alias";
const FIELD_DURATION_MS: &str = "duration_ms";
const FIELD_TRACE_ID: &str = "trace_id";
const FIELD_SPAN_ID: &str = "span_id";
const FIELD_MESSAGE: &str = "message";
const FIELD_TARGET_OVERRIDE_PREFIX: &str = "zeroclaw_log_internal";

/// tracing-subscriber Layer that emits LogEvents into zeroclaw-log AND
/// captures span-context attribution from span attributes on span
/// creation. The captured attribution is the single typed
/// [`ZeroclawAttribution`] — no per-field marker types.
pub struct LogCaptureLayer;

impl<S> tracing_subscriber::Layer<S> for LogCaptureLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = FieldCollector::default();
        attrs.record(&mut visitor);
        visitor.finalize();
        install_span_marker(&visitor, id, &ctx);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut visitor = FieldCollector::default();
        values.record(&mut visitor);
        visitor.finalize();
        install_span_marker(&visitor, id, &ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();

        if target.starts_with(FIELD_TARGET_OVERRIDE_PREFIX) {
            return;
        }

        let severity = Severity::from_tracing_level(*metadata.level());

        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        visitor.finalize();

        let category = visitor
            .category
            .as_deref()
            .and_then(EventCategory::parse)
            .or_else(|| {
                // Walk leaf→root for the closest enclosing-span category.
                ctx.lookup_current()
                    .into_iter()
                    .flat_map(|span| span.scope())
                    .find_map(|span| span.extensions().get::<SpanCategory>().map(|c| c.0))
            })
            .unwrap_or(EventCategory::Internal);

        let action = visitor
            .action
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| metadata.name().to_string());

        let mut log_event = LogEvent::new(severity, &action, category);

        if let Some(outcome) = visitor.outcome.as_deref().and_then(EventOutcome::parse) {
            log_event.set_outcome(outcome);
        }

        log_event.message = Some(visitor.message.unwrap_or_default());
        log_event.trace_id = visitor.trace_id;
        log_event.span_id = visitor.span_id;
        log_event.zeroclaw = visitor.attribution;

        if !visitor.extra.is_empty() {
            log_event.attributes = Value::Object(visitor.extra);
        }

        // Recover attribution from span context for any field the event
        // didn't explicitly set. Walks parent spans and merges the single
        // `ZeroclawAttribution` marker each carries.
        if let Some(span_ref) = ctx.lookup_current() {
            let mut current = Some(span_ref);
            while let Some(span) = current {
                let exts = span.extensions();
                if let Some(parent) = exts.get::<ZeroclawAttribution>() {
                    log_event.zeroclaw.merge_from(parent);
                }
                drop(exts);
                if log_event.zeroclaw.is_fully_populated() {
                    break;
                }
                current = span.parent();
            }
        }

        record_event(log_event);
    }
}

#[derive(Default)]
struct FieldCollector {
    action: Option<String>,
    category: Option<String>,
    outcome: Option<String>,
    trace_id: Option<String>,
    span_id: Option<String>,
    message: Option<String>,
    attribution: ZeroclawAttribution,
    /// Fallback `agent` field — applied as `agent_alias` only when
    /// `agent_alias` itself wasn't recorded on the same event.
    agent_fallback: Option<String>,
    extra: JsonMap<String, Value>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field.name(), Value::String(value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field(field.name(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field(field.name(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == FIELD_DURATION_MS {
            self.attribution.duration_ms = Some(value);
            return;
        }
        self.record_field(field.name(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_field(
            field.name(),
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        );
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut buf = String::new();
        let _ = write!(&mut buf, "{value}");
        let mut current = value.source();
        while let Some(src) = current {
            let _ = write!(&mut buf, ": {src}");
            current = src.source();
        }
        self.record_field(field.name(), Value::String(buf));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut buf = String::new();
        let _ = write!(&mut buf, "{value:?}");
        if field.name() == FIELD_MESSAGE {
            self.message = Some(strip_outer_quotes(&buf));
            return;
        }
        self.record_field(field.name(), Value::String(buf));
    }
}

impl FieldCollector {
    /// Apply post-visit resolutions: bare `agent = "..."` becomes
    /// `agent_alias` only when `agent_alias` wasn't recorded explicitly.
    fn finalize(&mut self) {
        if self.attribution.get("agent_alias").is_none()
            && let Some(alias) = self.agent_fallback.take()
        {
            self.attribution.set("agent_alias", alias);
        }
    }

    fn record_field(&mut self, name: &str, value: Value) {
        match name {
            ACTION_FIELD => {
                if let Value::String(s) = value {
                    self.action = Some(s);
                }
                return;
            }
            CATEGORY_FIELD => {
                if let Value::String(s) = value {
                    self.category = Some(s);
                }
                return;
            }
            FIELD_OUTCOME => {
                if let Value::String(s) = value {
                    self.outcome = Some(s);
                }
                return;
            }
            FIELD_TRACE_ID => {
                if let Value::String(s) = value {
                    self.trace_id = Some(s);
                }
                return;
            }
            FIELD_SPAN_ID => {
                if let Value::String(s) = value {
                    self.span_id = Some(s);
                }
                return;
            }
            FIELD_MESSAGE => {
                if let Value::String(s) = value {
                    self.message = Some(s);
                }
                return;
            }
            FIELD_AGENT => {
                if let Value::String(s) = value {
                    self.agent_fallback = Some(s);
                }
                return;
            }
            FIELD_PARENT_ALIAS => {
                if let Value::String(s) = value {
                    self.attribution.set("agent_alias", s);
                }
                return;
            }
            _ => {}
        }

        // Composite prefix? When the value carries an alias suffix
        // (`<type>.<alias>`), split it into the three composite keys.
        // When it's bare (no dot) we treat it as type-only — set just
        // `<prefix>_type` so it doesn't clobber a parent span's full
        // composite via the leaf→root merge in `on_event`.
        for prefix in COMPOSITE_PREFIXES {
            if name == *prefix
                && let Value::String(s) = &value
            {
                if s.contains('.') {
                    self.attribution.set_composite(prefix, s);
                } else {
                    self.attribution.set(format!("{prefix}_type"), s.clone());
                }
                return;
            }
        }

        // Plain attribution field?
        if ATTRIBUTION_FIELDS.contains(&name)
            && let Value::String(s) = value
        {
            self.attribution.set(name, s);
            return;
        }

        // Everything else lands in the free-form attributes map.
        self.extra.insert(name.to_string(), value);
    }
}

fn install_span_marker<S>(visitor: &FieldCollector, id: &Id, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let Some(span) = ctx.span(id) else {
        return;
    };

    // Attribution: merge with any existing marker so re-recording on the
    // same span never clears a previously-stamped key.
    if !visitor.attribution.fields.is_empty() || visitor.attribution.duration_ms.is_some() {
        let mut attribution = visitor.attribution.clone();
        let exts_read = span.extensions();
        if let Some(existing) = exts_read.get::<ZeroclawAttribution>() {
            attribution.merge_from(existing);
        }
        drop(exts_read);
        span.extensions_mut().insert(attribution);
    }

    // Category: stash the closest enclosing-span category so descendants
    // inherit. Routed through the same span-walk machinery as attribution.
    if let Some(category) = visitor
        .category
        .as_deref()
        .and_then(EventCategory::parse)
    {
        span.extensions_mut().insert(SpanCategory(category));
    }
}

fn strip_outer_quotes(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        return trimmed[1..trimmed.len() - 1].to_string();
    }
    trimmed.to_string()
}
