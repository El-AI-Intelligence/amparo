//! Routing approval button presses back to the waiting gate.
//!
//! The gate registers the `(chat_id, approval_id)` pair — along with the
//! requester's user id — when it sends an approval message and waits on the
//! returned receiver. A button press calls [`ApprovalRouter::take`], which
//! checks the presser against the requester: only the requester's own press
//! atomically removes the sender. A second press on the same approval is
//! [`TakeResult::AlreadyDecided`], and a press from anyone else is
//! [`TakeResult::WrongUser`] with the entry kept, so the requester's later
//! press still routes. [`ApprovalRouter::unregister`] removes an entry and
//! notifies the waiter with `false` when no decision can come from it
//! anymore.

use std::collections::HashMap;
use tokio::sync::{oneshot, Mutex};

/// Result of [`ApprovalRouter::take`].
#[derive(Debug)]
pub enum TakeResult {
    /// The press was accepted; the decision sender is delivered.
    Routed(oneshot::Sender<bool>),
    /// No entry — already decided.
    AlreadyDecided,
    /// Entry exists but the presser is not the requester (entry kept).
    WrongUser,
}

/// Routes approval button presses to the gate that is waiting for them.
///
/// Keyed by `(chat_id, approval_id)` — the approval id is the tool call id,
/// unique per task, so one task per chat never collides. The value is the
/// requester's user id paired with the decision sender, so a press can only
/// ever be routed to the gate by the user who started the task. All
/// operations are atomic against the map; no lock is held across an await.
pub struct ApprovalRouter {
    pending: Mutex<HashMap<(String, String), (String, oneshot::Sender<bool>)>>,
}

impl ApprovalRouter {
    /// An empty router.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a waiting approval and return the receiver the gate awaits.
    ///
    /// `requester_user_id` is the platform id of the user whose press may
    /// decide — any other press is refused without consuming the entry.
    /// Registering a key that already exists supersedes the earlier waiter:
    /// its receiver resolves `Err`, which the gate reports as superseded.
    pub async fn register(
        &self,
        chat_id: &str,
        approval_id: &str,
        requester_user_id: &str,
    ) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            (chat_id.to_string(), approval_id.to_string()),
            (requester_user_id.to_string(), tx),
        );
        rx
    }

    /// Hand the decision sender to the requester, atomically.
    ///
    /// The requester's first press gets the sender; a second press is
    /// [`TakeResult::AlreadyDecided`] (the double-press safety). A press
    /// from a different user is [`TakeResult::WrongUser`] and the entry
    /// STAYS — only the gate's timeout [`unregister`](ApprovalRouter::unregister)
    /// or the requester's own press removes it.
    pub async fn take(
        &self,
        chat_id: &str,
        approval_id: &str,
        presser_user_id: &str,
    ) -> TakeResult {
        let key = (chat_id.to_string(), approval_id.to_string());
        let mut pending = self.pending.lock().await;
        let Some((requester, _)) = pending.get(&key) else {
            return TakeResult::AlreadyDecided;
        };
        if requester != presser_user_id {
            return TakeResult::WrongUser;
        }
        let (_, tx) = pending
            .remove(&key)
            .expect("the entry was inspected just above");
        TakeResult::Routed(tx)
    }

    /// Remove the entry, notifying a still-waiting gate with `false`.
    ///
    /// Returns whether an entry was present. The notify is best-effort: if
    /// the gate already dropped its receiver (say, it timed out a moment
    /// ago), the send fails and is ignored.
    pub async fn unregister(&self, chat_id: &str, approval_id: &str) -> bool {
        let key = (chat_id.to_string(), approval_id.to_string());
        if let Some((_, tx)) = self.pending.lock().await.remove(&key) {
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
        let rx = router.register("chat_1", "call_1", "user_1").await;
        let tx = match router.take("chat_1", "call_1", "user_1").await {
            TakeResult::Routed(tx) => tx,
            other => panic!("expected the requester's press to route, got {other:?}"),
        };
        tx.send(true).unwrap();
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn double_take_is_already_decided() {
        let router = ApprovalRouter::new();
        router.register("chat_1", "call_1", "user_1").await;
        assert!(matches!(
            router.take("chat_1", "call_1", "user_1").await,
            TakeResult::Routed(_)
        ));
        assert!(
            matches!(
                router.take("chat_1", "call_1", "user_1").await,
                TakeResult::AlreadyDecided
            ),
            "second press is already decided"
        );
    }

    #[tokio::test]
    async fn unregister_notifies_false() {
        let router = ApprovalRouter::new();
        let rx = router.register("chat_1", "call_1", "user_1").await;
        assert!(router.unregister("chat_1", "call_1").await);
        assert_eq!(rx.await.unwrap(), false, "unregistered gate is told denied");
    }

    #[tokio::test]
    async fn unregister_of_missing_entry_returns_false() {
        let router = ApprovalRouter::new();
        assert!(!router.unregister("chat_1", "call_1").await);
    }

    #[tokio::test]
    async fn wrong_user_press_is_rejected_and_keeps_the_entry() {
        let router = ApprovalRouter::new();
        router.register("chat_1", "call_1", "alice").await;
        assert!(matches!(
            router.take("chat_1", "call_1", "bob").await,
            TakeResult::WrongUser
        ));
        assert!(
            matches!(
                router.take("chat_1", "call_1", "alice").await,
                TakeResult::Routed(_)
            ),
            "the requester's own later press must still route"
        );
    }

    #[tokio::test]
    async fn wrong_user_then_unregister_notifies_false() {
        let router = ApprovalRouter::new();
        let rx = router.register("chat_1", "call_1", "alice").await;
        assert!(matches!(
            router.take("chat_1", "call_1", "bob").await,
            TakeResult::WrongUser
        ));
        assert!(router.unregister("chat_1", "call_1").await);
        assert_eq!(rx.await.unwrap(), false, "unregistered gate is told denied");
    }
}
