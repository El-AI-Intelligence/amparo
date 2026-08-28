//! Terminal event rendering: one `[tag]` line per agent event on stderr.
//!
//! stdout is reserved for the final answer, so every observability line
//! (turn, gate, approval, execution, verification, report) goes to stderr.
//! The canonical `[tag]` line format lives in
//! [`amparo_agent::format_event`] — chat adapters forward the same lines to
//! their platforms, so every face of the agent shows identical text.

use amparo_agent::{format_event, AgentEvent, EventSink};

/// Sink that renders each event as a stderr line.
pub struct PrintingSink;

impl EventSink for PrintingSink {
    fn emit(&self, event: &AgentEvent) {
        eprintln!("{}", format_event(event));
    }
}
