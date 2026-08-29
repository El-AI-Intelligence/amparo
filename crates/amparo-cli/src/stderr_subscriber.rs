//! The minimal stderr subscriber amparo installs at startup.
//!
//! amparo has no logging framework: the agent signals trouble with
//! `tracing::warn!` (a checkpoint save failure, a corrupt session file)
//! and this ~50-line [`tracing::subscriber::Subscriber`] prints exactly
//! those `warn`/`error` events to stderr, where the operator is watching —
//! each event already carries its own `[tag]`, so the line is printed
//! as-is. `info`/`debug` stay silent: progress lines own the stderr
//! channel.

use std::fmt;

use tracing::event::Event;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Subscriber;
use tracing::Level;

/// The one subscriber the binary installs: warn and error events only,
/// each as a single stderr line.
pub struct StderrWarnSubscriber;

impl Subscriber for StderrWarnSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.level() <= &Level::WARN
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut message = String::new();
        event.record(&mut Message(&mut message));
        if !message.is_empty() {
            eprintln!("{message}");
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Captures the event's `message` field (tracing pre-formats it when the
/// macro carries format arguments).
struct Message<'a>(&'a mut String);

impl Visit for Message<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}
