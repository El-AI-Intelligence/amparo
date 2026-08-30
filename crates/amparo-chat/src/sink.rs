//! The event sink that forwards agent events into the chat.
//!
//! [`ChatEventSink`] renders each [`AgentEvent`] with
//! [`amparo_agent::format_event`] and forwards it into a bounded per-task
//! outbox channel; a spawned [`crate::transport::drain_outbox`] task
//! delivers the lines. The channel is bounded (64) and `emit` uses
//! `try_send`, so a slow chat can never stall the agent loop — a full
//! outbox just drops the line. The final answer deliberately bypasses this
//! sink (the driver sends it directly), so it can never be lost.

use amparo_agent::{AgentEvent, EventSink};
use std::sync::Arc;
use tokio::sync::mpsc;

/// An [`EventSink`] forwarding each event, formatted, into a chat outbox.
pub struct ChatEventSink {
    tx: mpsc::Sender<String>,
}

impl ChatEventSink {
    /// A fresh outbox pair: the sink to hand the agent, and the receiver a
    /// spawned [`crate::transport::drain_outbox`] task drains into the chat.
    ///
    /// The channel is bounded to 64 lines; `emit` never blocks, so a full
    /// outbox drops new lines rather than stalling the loop.
    pub fn channel() -> (Arc<Self>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(64);
        (Arc::new(Self { tx }), rx)
    }
}

impl EventSink for ChatEventSink {
    fn emit(&self, event: &AgentEvent) {
        // Best effort: try_send never blocks, and a full outbox drops the
        // line. The final answer does not flow through here at all.
        let _ = self.tx.try_send(amparo_agent::format_event(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_render_through_the_channel() {
        let (sink, mut rx) = ChatEventSink::channel();
        sink.emit(&AgentEvent::TaskStarted {
            prompt: "hi".into(),
            task_id: None,
        });
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "done".into(),
            task_id: None,
        });
        assert_eq!(rx.recv().await.as_deref(), Some("[task] hi"));
        assert_eq!(rx.recv().await.as_deref(), Some("[complete] done"));
    }

    #[tokio::test]
    async fn full_channel_drops_new_lines_without_blocking() {
        let (sink, mut rx) = ChatEventSink::channel();
        // One more line than the 64-line buffer: emit is sync, so if it ever
        // blocked this test would hang instead of passing.
        for i in 0..65 {
            sink.emit(&AgentEvent::TaskStarted {
                prompt: format!("msg {i}"),
                task_id: None,
            });
        }
        let mut received = Vec::new();
        while let Ok(line) = rx.try_recv() {
            received.push(line);
        }
        assert_eq!(received.len(), 64, "the buffer holds exactly its capacity");
        assert!(received[0].contains("msg 0"), "lines arrive in order");
        assert!(
            !received.iter().any(|l| l.contains("msg 64")),
            "the overflow line is dropped, not queued"
        );
    }
}
