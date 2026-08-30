//! Slack adapter — Socket Mode (websocket), Block Kit buttons, REST outbound.
//!
//! `amparo chat slack` runs this adapter. The Slack app needs Socket Mode
//! enabled with the `connections:write` scope (the app token authenticates
//! `apps.connections.open`, which yields the per-connection websocket URL)
//! and a bot token with `chat:write` for the REST calls
//! (`chat.postMessage`, `chat.update`). Both tokens come from the
//! environment — `AMPARO_CHAT_SLACK_APP_TOKEN` and
//! `AMPARO_CHAT_SLACK_BOT_TOKEN` — read fail-closed by [`serve`] and never
//! logged or echoed anywhere.
//!
//! Wire protocol:
//!
//! - Inbound is a websocket. Every envelope is acked **immediately on read**
//!   with `{"envelope_id": "…"}` — Slack retries unacked envelopes after
//!   ~3s, so acking first and processing second is the receive-loop
//!   contract. A `disconnect` envelope is acked and then re-opens the
//!   connection; unknown envelope types are acked and ignored.
//! - `events_api` messages become driver input — except bot-originated
//!   messages (`bot_id`) and message subtypes, which are the adapter's own
//!   echo and are skipped (the echo-loop kill).
//! - `interactive` `block_actions` presses become
//!   [`ApprovalButtonPress`]es; a press that reaches no waiting gate
//!   (already decided) replaces the message through the payload's
//!   `response_url` so its stale buttons stop dangling, and a press from a
//!   user who did not start the task gets an ephemeral toast on that same
//!   `response_url` — the requester's buttons stay.
//! - Outbound: `chat.postMessage` for text (truncated to 39000 chars,
//!   Slack's message limit) and for Block Kit approval messages carrying
//!   Approve/Deny buttons (`approve:<call_id>` / `deny:<call_id>`);
//!   `chat.update` for gate edits (blocks emptied so the buttons vanish).
//!
//! Single-operator M4: one driver shared by every chat, as in the rest of
//! the crate.

use crate::driver::ChatDriver;
use crate::transport::{
    ApprovalButtonPress, ApprovalMessage, ChatError, ChatRef, ChatTransport, PressOutcome,
};
use amparo_agent::ApprovalRequest;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// `chat.postMessage` text is limited to 39000 chars — Slack's message
/// limit — so long agent output is truncated before sending.
const TEXT_LIMIT: usize = 39000;

/// Block text objects are limited to 3000 chars.
const BLOCK_TEXT_LIMIT: usize = 3000;

/// Pause before re-opening a dropped Socket Mode connection, so a
/// persistently dying socket cannot spin.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// A connected Socket Mode websocket.
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One Socket Mode frame. `envelope_id` is absent for the greeting
/// (`{"type":"hello"}`), which needs no ack.
#[derive(Deserialize)]
struct Envelope {
    envelope_id: Option<String>,
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    payload: Option<Value>,
}

/// The `events_api` payload: a message event carries channel/user/text;
/// bot-originated messages carry `bot_id` instead of `user`.
#[derive(Deserialize)]
struct EventPayload {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

/// The `interactive` payload: a `block_actions` press with the button's
/// `action_id` (`approve:<call_id>` / `deny:<call_id>`), the user who
/// pressed, the channel the message lives in, and the `response_url` for
/// cleanup edits.
#[derive(Deserialize)]
struct InteractivePayload {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    actions: Vec<Action>,
    #[serde(default)]
    user: Option<InteractionUser>,
    #[serde(default)]
    channel: Option<ChannelId>,
    #[serde(default)]
    response_url: Option<String>,
}

/// One block button press.
#[derive(Deserialize)]
struct Action {
    action_id: String,
}

/// The user who pressed a button — Slack's raw user id.
#[derive(Deserialize)]
struct InteractionUser {
    id: String,
}

/// The channel id of an interactive message.
#[derive(Deserialize)]
struct ChannelId {
    id: String,
}

/// The Slack adapter: Socket Mode inbound, `chat.postMessage` /
/// `chat.update` outbound.
///
/// `base` is the Web API root (`https://slack.com/api` in production;
/// integration tests point it at a local mock), with `apps.connections.open`
/// and the chat methods appended. The app token authenticates
/// `apps.connections.open` (which yields the per-connection websocket URL);
/// the bot token authenticates every REST call. Neither token is ever
/// logged — they enter through [`serve`]'s environment read, and tests
/// inject fakes directly.
pub struct SlackTransport {
    base: String,
    app_token: String,
    bot_token: String,
    http: reqwest::Client,
}

impl SlackTransport {
    /// Build a transport rooted at `base` (the real API —
    /// `https://slack.com/api` — or a mock in tests) with the app and bot
    /// tokens and the HTTP client to use.
    pub fn new(
        base: impl Into<String>,
        app_token: String,
        bot_token: String,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base: base.into(),
            app_token,
            bot_token,
            http,
        }
    }

    /// Open one Socket Mode connection: ask `apps.connections.open` for a
    /// websocket URL and connect to it. An `ok: false` answer is fatal —
    /// the app token is rejected — and so is any transport failure, so the
    /// serve loop exits rather than retrying a dead token.
    async fn open_connection(&self) -> Result<Socket, ChatError> {
        let resp: Value = self
            .http
            .post(format!("{}/apps.connections.open", self.base))
            .bearer_auth(&self.app_token)
            .form(&[("token", self.app_token.as_str())])
            .send()
            .await
            .map_err(|e| ChatError::Http(format!("apps.connections.open: {e}")))?
            .json()
            .await
            .map_err(|e| ChatError::Http(format!("apps.connections.open: {e}")))?;
        let ok = resp.get("ok").and_then(Value::as_bool).unwrap_or(false);
        match (ok, resp.get("url").and_then(Value::as_str)) {
            (true, Some(url)) => {
                let (ws, _) = connect_async(url)
                    .await
                    .map_err(|e| ChatError::Ws(format!("slack socket: {e}")))?;
                Ok(ws)
            }
            _ => Err(ChatError::Fatal(format!(
                "apps.connections.open failed: {}",
                resp.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ))),
        }
    }

    /// One POST to the Slack Web API: bot-token Bearer auth, JSON body,
    /// JSON response.
    async fn api_post(&self, method: &str, body: &Value) -> Result<Value, ChatError> {
        self.http
            .post(format!("{}/{}", self.base, method))
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .await
            .map_err(|e| ChatError::Http(format!("{method}: {e}")))?
            .json()
            .await
            .map_err(|e| ChatError::Http(format!("{method}: {e}")))
    }

    /// Run one Socket Mode connection: read envelopes, ack each immediately,
    /// then process. Returns when the socket ends or a `disconnect` envelope
    /// has been acked — the caller re-opens a fresh connection.
    async fn run_connection(&self, ws: Socket, driver: &ChatDriver) {
        let (mut write, mut read) = ws.split();
        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(text)) => text,
                // A close frame or any socket error ends this connection;
                // the outer loop re-opens one.
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue, // ping/pong/binary — irrelevant here
            };
            let Ok(envelope) = serde_json::from_str::<Envelope>(text.as_str()) else {
                continue; // not a parseable envelope — nothing to ack or route
            };
            // ACK IMMEDIATELY ON READ — Slack retries unacked envelopes
            // after ~3s, so acking before processing is the whole contract.
            if let Some(id) = envelope.envelope_id.as_deref() {
                let ack = json!({"envelope_id": id});
                if write.send(Message::text(ack.to_string())).await.is_err() {
                    break; // the socket is gone — nothing else to do here
                }
            }
            match envelope.r#type.as_str() {
                "hello" => {}          // acked (when it carries an id) and ignored
                "disconnect" => break, // acked — the caller re-opens
                "events_api" => self.handle_event(envelope.payload, driver).await,
                "interactive" => self.handle_interactive(envelope.payload, driver).await,
                _ => {} // unknown — acked (so Slack stops retrying), ignored
            }
        }
    }

    /// Route one `events_api` envelope payload to the driver. Messages
    /// carrying a `bot_id` (the adapter's own posts) or a `subtype`
    /// (edits, replies, …) are the echo — already acked, skipped here.
    async fn handle_event(&self, payload: Option<Value>, driver: &ChatDriver) {
        let Some(payload) = payload else { return };
        let Ok(event) = serde_json::from_value::<EventPayload>(payload) else {
            return;
        };
        if event.r#type != "message" || event.bot_id.is_some() || event.subtype.is_some() {
            return;
        }
        let (Some(channel), Some(user), Some(text)) = (event.channel, event.user, event.text)
        else {
            return; // not addressable as a chat message
        };
        if text.is_empty() {
            return;
        }
        let chat = ChatRef {
            platform: "slack",
            chat_id: channel,
            user_id: user,
        };
        driver.on_message(chat, text).await;
    }

    /// Route one `interactive` envelope payload: a `block_actions` press
    /// becomes an [`ApprovalButtonPress`] (`approve:<id>` / `deny:<id>`).
    /// A press that reaches no waiting gate — already decided, or never
    /// registered — replaces the message via the payload's `response_url`
    /// so the stale buttons stop dangling.
    async fn handle_interactive(&self, payload: Option<Value>, driver: &ChatDriver) {
        let Some(payload) = payload else { return };
        let Ok(interactive) = serde_json::from_value::<InteractivePayload>(payload) else {
            return;
        };
        if interactive.r#type != "block_actions" {
            return;
        }
        let Some(action) = interactive.actions.first() else {
            return;
        };
        let Some((kind, approval_id)) = action.action_id.split_once(':') else {
            return;
        };
        let Some(channel_id) = interactive.channel.map(|c| c.id) else {
            return;
        };
        let approved = match kind {
            "approve" => true,
            "deny" => false,
            _ => return,
        };
        let press = ApprovalButtonPress {
            chat_id: channel_id,
            approval_id: approval_id.to_string(),
            approved,
            user_id: interactive.user.map(|u| u.id).unwrap_or_default(),
        };
        match driver.on_approval(press).await {
            PressOutcome::Routed => {}
            PressOutcome::AlreadyDecided => {
                // Already decided — replace the message so its buttons vanish.
                if let Some(url) = interactive.response_url {
                    let body = json!({"replace_original": true, "text": "Already decided"});
                    let _ = self.http.post(url).json(&body).send().await;
                }
            }
            PressOutcome::WrongUser => {
                // The requester's buttons must stay — an ephemeral toast,
                // never a replace_original.
                if let Some(url) = interactive.response_url {
                    let body = json!({
                        "response_type": "ephemeral",
                        "text": crate::driver::WRONG_USER_TOAST,
                    });
                    let _ = self.http.post(url).json(&body).send().await;
                }
            }
        }
    }
}

#[async_trait]
impl ChatTransport for SlackTransport {
    async fn send_text(&self, chat: &ChatRef, text: &str) -> Result<(), ChatError> {
        let body = json!({"channel": chat.chat_id, "text": truncate(text, TEXT_LIMIT)});
        let resp = self.api_post("chat.postMessage", &body).await?;
        if resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(ChatError::Slack(api_error("chat.postMessage", &resp)))
        }
    }

    async fn send_approval(
        &self,
        chat: &ChatRef,
        request: &ApprovalRequest,
        approval_id: &str,
    ) -> Result<ApprovalMessage, ChatError> {
        // One section block (tool, arguments, reasons — block text is
        // limited to 3000 chars) and one actions block with the two
        // buttons; the top-level text is the non-rich fallback.
        use crate::transport::{preflight_line, session_line};
        let args = truncate(&request.arguments.to_string(), BLOCK_TEXT_LIMIT);
        let session = session_line(request)
            .map(|line| format!("{line}\n"))
            .unwrap_or_default();
        let preflight = preflight_line(request)
            .map(|line| format!("{line}\n"))
            .unwrap_or_default();
        let reasons = request.reasons.join("\n");
        let section_text = truncate(
            &format!(
                "{session}*{}* needs approval\n```{args}```\n{preflight}{reasons}",
                request.tool_name
            ),
            BLOCK_TEXT_LIMIT,
        );
        let body = json!({
            "channel": chat.chat_id,
            "text": format!("Approval required for {}", request.tool_name),
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn", "text": section_text}},
                {"type": "actions", "elements": [
                    {"type": "button", "text": {"type": "plain_text", "text": "Approve"},
                     "style": "primary", "action_id": format!("approve:{approval_id}")},
                    {"type": "button", "text": {"type": "plain_text", "text": "Deny"},
                     "style": "danger", "action_id": format!("deny:{approval_id}")},
                ]},
            ],
        });
        let resp = self.api_post("chat.postMessage", &body).await?;
        if resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            match resp.get("ts").and_then(Value::as_str) {
                Some(ts) => Ok(ApprovalMessage {
                    chat_id: chat.chat_id.clone(),
                    message_id: ts.to_string(),
                }),
                None => Err(ChatError::Slack(
                    "chat.postMessage accepted without a ts".into(),
                )),
            }
        } else {
            Err(ChatError::Slack(api_error("chat.postMessage", &resp)))
        }
    }

    async fn edit_approval(&self, msg: &ApprovalMessage, outcome: &str) -> Result<(), ChatError> {
        // Empty blocks remove the buttons; `text` records the outcome.
        let body =
            json!({"channel": msg.chat_id, "ts": msg.message_id, "text": outcome, "blocks": []});
        let resp = self.api_post("chat.update", &body).await?;
        if resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(())
        } else {
            Err(ChatError::Slack(api_error("chat.update", &resp)))
        }
    }

    async fn receive(self: Arc<Self>, driver: Arc<ChatDriver>) -> Result<(), ChatError> {
        loop {
            let ws = self.open_connection().await?;
            self.run_connection(ws, &driver).await;
            // The socket ended (a `disconnect` envelope, a close frame, or
            // an error) — Slack expects a fresh `apps.connections.open`.
            // Pause so a persistently dying socket cannot spin.
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }
}

/// The Slack API `error` field of an `ok: false` body, or a fallback.
fn api_error(method: &str, resp: &Value) -> String {
    let error = resp
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    format!("{method} rejected: {error}")
}

/// Truncate `s` to `max` chars, appending `…` when anything was cut.
fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Serve the Slack adapter until the process is interrupted.
///
/// Reads `AMPARO_CHAT_SLACK_APP_TOKEN` and `AMPARO_CHAT_SLACK_BOT_TOKEN`
/// fail-closed — either missing means `Err(Fatal)` before anything else
/// runs. The driver comes from [`crate::dispatch::build_driver`] (which
/// takes this platform's transport — the driver holds the outbound
/// handle); the receive loop then runs until a fatal error (a rejected app
/// token, for example) ends it.
pub async fn serve(flags: &crate::dispatch::ChatFlags) -> Result<(), ChatError> {
    let app_token = std::env::var("AMPARO_CHAT_SLACK_APP_TOKEN").map_err(|_| {
        ChatError::Fatal(
            "AMPARO_CHAT_SLACK_APP_TOKEN (and AMPARO_CHAT_SLACK_BOT_TOKEN) is required".into(),
        )
    })?;
    let bot_token = std::env::var("AMPARO_CHAT_SLACK_BOT_TOKEN").map_err(|_| {
        ChatError::Fatal(
            "AMPARO_CHAT_SLACK_APP_TOKEN (and AMPARO_CHAT_SLACK_BOT_TOKEN) is required".into(),
        )
    })?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ChatError::Http(format!("http client: {e}")))?;
    let transport = Arc::new(SlackTransport::new(
        "https://slack.com/api",
        app_token,
        bot_token,
        http,
    ));
    let driver = crate::dispatch::build_driver(flags, transport.clone())
        .await
        .map_err(|e| ChatError::Fatal(e.message))?;
    transport.receive(driver).await
}
