//! Routing approval button presses back to the waiting gate.
//!
//! The gate registers the `(chat_id, approval_id)` pair when it sends an
//! approval message and waits on the returned receiver. A button press calls
//! [`ApprovalRouter::take`], which atomically removes the sender — the
//! second press on the same approval gets `None`, which the platform adapter
//! reports as "already decided". [`ApprovalRouter::unregister`] removes an
//! entry and notifies the waiter with `false` when no decision can come from
//! it anymore.

use std::collections::HashMap;
use tokio::sync::{oneshot, Mutex};

/// Routes approval button presses to the gate that is waiting for them.
///
/// Keyed by `(chat_id, approval_id)` — the approval id is the tool call id,
/// unique per task, so one task per chat never collides. All operations are
/// atomic against the map; no lock is held across an await.
pub struct ApprovalRouter {
    pending: Mutex<HashMap<(String, String), oneshot::Sender<bool>>>,
}

impl ApprovalRouter {
    /// An empty router.
    pub fn new() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }

    /// Register a waiting approval and return the receiver the gate awaits.
    ///
    /// Registering a key that already exists supersedes the earlier waiter:
    /// its receiver resolves `Err`, which the gate reports as superseded.
    pub async fn register(&self, chat_id: &str, approval_id: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert((chat_id.to_string(), approval_id.to_string()), tx);
        rx
    }

    /// Atomically remove the sender for this approval.
    ///
    /// This is the double-press safety: the first press gets the sender, the
    /// second returns `None` and is reported as already decided.
    pub async fn take(&self, chat_id: &str, approval_id: &str) -> Option<oneshot::Sender<bool>> {
        self.pending
            .lock()
            .await
            .remove(&(chat_id.to_string(), approval_id.to_string()))
    }

    /// Remove the entry, notifying a still-waiting gate with `false`.
    ///
    /// Returns whether an entry was present. The notify is best-effort: if
    /// the gate already dropped its receiver (say, it timed out a moment
    /// ago), the send fails and is ignored.
    pub async fn unregister(&self, chat_id: &str, approval_id: &str) -> bool {
        let key = (chat_id.to_string(), approval_id.to_string());
        if let Some(tx) = self.pending.lock().await.remove(&key) {
            let _ = tx.send(false);
            true
        } else {
            false
        }
    }
}

impl Default for ApprovalRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_take_roundtrip() {
        let router = ApprovalRouter::new();
        let rx = router.register("chat_1", "call_1").await;
        let tx = router.take("chat_1", "call_1").await.expect("registered sender");
        tx.send(true).unwrap();
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn double_take_returns_none() {
        let router = ApprovalRouter::new();
        router.register("chat_1", "call_1").await;
        assert!(router.take("chat_1", "call_1").await.is_some());
        assert!(router.take("chat_1", "call_1").await.is_none(), "second press is already decided");
    }

    #[tokio::test]
    async fn unregister_notifies_false() {
        let router = ApprovalRouter::new();
        let rx = router.register("chat_1", "call_1").await;
        assert!(router.unregister("chat_1", "call_1").await);
        assert_eq!(rx.await.unwrap(), false, "unregistered gate is told denied");
    }

    #[tokio::test]
    async fn unregister_of_missing_entry_returns_false() {
        let router = ApprovalRouter::new();
        assert!(!router.unregister("chat_1", "call_1").await);
    }
}
