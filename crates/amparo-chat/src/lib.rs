//! Amparo Chat — the chat adapter layer: one transport seam, three platforms.
//!
//! M4 gives the agent a chat face. This crate owns everything platform
//! neutral so the per-platform adapters stay thin:
//!
//! - [`transport::ChatTransport`] is the seam every adapter implements —
//!   text out, approval messages with inline buttons, approval edits, and a
//!   receive loop that feeds a [`driver::ChatDriver`]. The normalized wire
//!   types ([`transport::ChatRef`], [`transport::IncomingMessage`],
//!   [`transport::ApprovalButtonPress`], [`transport::ApprovalMessage`])
//!   live beside it.
//! - [`driver::ChatDriver`] turns a normalized message into a per-task
//!   agent run: allowlist check, one task per chat, a fresh
//!   [`amparo_agent::Agent`] per task built from the shared parts, and a
//!   panic-proof task boundary.
//! - [`gate::ChatApprovalGate`] asks a human via inline Approve/Deny
//!   buttons and auto-denies on timeout; [`router::ApprovalRouter`] routes
//!   the button press back to the waiting gate — only the requester's own
//!   press is accepted (a second press on the same approval is already
//!   decided, a foreign press is refused with a toast and keeps the entry
//!   pending).
//! - [`sink::ChatEventSink`] forwards the agent's events into the chat as
//!   best-effort progress lines; the final answer bypasses the sink
//!   entirely, so it can never be lost.
//!
//! The Telegram, Discord and Slack adapters land in later commits
//! ([`telegram`], [`discord`], [`slack`]); [`dispatch`] hosts them behind
//! the `amparo chat` subcommand.

#![warn(missing_docs)]

// The crate name is hyphenated (`amparo-chat`), so the crate cannot refer to
// itself by name unless it declares the alias — unit tests include
// `tests/common/mod.rs`, which imports `amparo_chat::...` exactly like the
// integration tests do.
extern crate self as amparo_chat;

pub mod config;
pub mod discord;
pub mod dispatch;
pub mod driver;
pub mod gate;
pub mod notification;
pub mod router;
pub mod schedule;
pub mod sink;
pub mod slack;
pub mod telegram;
pub mod transport;

pub use config::{ChatConfig, ConfigError, UserProfile};
pub use driver::{ChatDriver, PolicySource, Tenants, SCHEDULE_GRACE};
pub use gate::{ChatApprovalGate, TimeoutApprovalGate};
pub use notification::ChatNotificationTransport;
pub use router::{ApprovalRouter, TakeResult};
pub use schedule::{
    due_scan, schedule_dir, JsonScheduleStore, ScheduleStore, ScheduleTool, ScheduledStatus,
    ScheduledTask,
};
pub use sink::ChatEventSink;
pub use transport::{
    ApprovalButtonPress, ApprovalMessage, ChatError, ChatRef, ChatTransport, IncomingMessage,
    PressOutcome,
};
