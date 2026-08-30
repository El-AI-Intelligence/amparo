//! The Discord adapter: a gateway websocket receive loop plus REST sends.
//!
//! Inbound comes from the gateway (`wss://gateway.discord.gg`): the client
//! IDENTIFYs with [`INTENTS`], answers HELLO with a heartbeat task (one
//! op 1 per interval — an op 11 ack missed twice in a row forces a
//! reconnect), and feeds MESSAGE_CREATE and INTERACTION_CREATE dispatches
//! to the [`crate::driver::ChatDriver`]. The connection survives outages
//! RESUME-first (op 6 carrying the session id and last sequence); an
//! invalid session or a failed resume falls back to a fresh IDENTIFY after
//! a 5s gap, and three consecutive full IDENTIFYs without a READY end in
//! [`ChatError::Fatal`] — a dead token must surface, never silently spin
//! against Discord's identify rate limit.
//!
//! Outbound goes over REST (`https://discord.com/api/v10` with
//! `Authorization: Bot {token}`): plain text, approval messages as a
//! message-component Approve/Deny button pair (`approve:<call_id>` /
//! `deny:<call_id>`), and approval edits that replace the buttons with the
//! outcome. Button presses arrive back on the gateway as interactions and
//! get a receiver-only `{"type": 6}` (DEFERRED_UPDATE_MESSAGE) callback —
//! no Authorization header, the interaction token in the URL is the
//! credential — before the decision is routed to the driver. Every REST
//! call retries once on HTTP 429 after sleeping `retry_after` seconds plus
//! a half-second margin.
//!
//! The token comes from `AMPARO_CHAT_DISCORD_TOKEN` only and is never
//! logged or printed — even the transport's `Debug` output redacts it.

use crate::dispatch::{build_driver, ChatFlags};
use crate::driver::ChatDriver;
use crate::transport::{
    ApprovalButtonPress, ApprovalMessage, ChatError, ChatRef, ChatTransport, IncomingMessage,
    PressOutcome,
};
use amparo_agent::ApprovalRequest;
use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// The Discord gateway endpoint — API version 10, JSON encoding.
pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// The Discord REST base URL.
pub const REST_BASE: &str = "https://discord.com/api/v10";

/// Gateway intents: GUILD_MESSAGES (`1<<9`), DIRECT_MESSAGES (`1<<12`)
/// and MESSAGE_CONTENT (`1<<15`).
pub const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);

/// Opcode 0 — a dispatched event; `t` names the event and `s` its sequence.
pub const OP_DISPATCH: u8 = 0;
/// Opcode 1 — heartbeat.
pub const OP_HEARTBEAT: u8 = 1;
/// Opcode 2 — identify.
pub const OP_IDENTIFY: u8 = 2;
/// Opcode 6 — resume.
pub const OP_RESUME: u8 = 6;
/// Opcode 7 — the gateway asks the client to reconnect.
pub const OP_RECONNECT: u8 = 7;
/// Opcode 9 — the session was invalidated.
pub const OP_INVALID_SESSION: u8 = 9;
/// Opcode 10 — HELLO, the gateway's heartbeat interval.
pub const OP_HELLO: u8 = 10;
/// Opcode 11 — heartbeat acknowledgement.
pub const OP_HEARTBEAT_ACK: u8 = 11;

/// Consecutive full IDENTIFYs without a READY or RESUMED end in a fatal
/// error — a dead token must surface, never spin against the identify
/// rate limit.
const MAX_IDENTIFIES: u32 = 3;

/// Discord's per-message content limit, in characters.
const MAX_MESSAGE_CHARS: usize = 2000;

/// How long a fresh connection may wait for the gateway's HELLO before the
/// outer loop reconnects.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// The concrete websocket stream `connect_async` produces (TLS or plain),
/// split into a read half and a write half shared between the reader and
/// the heartbeat task.
type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
/// The gateway socket's read half.
type WsRead = SplitStream<WsStream>;
/// The gateway socket's write half.
type WsWrite = SplitSink<WsStream, WsMessage>;

/// Gateway session state that survives reconnects.
#[derive(Debug, Default)]
struct GatewayState {
    /// The session id from READY — the RESUME credential.
    session_id: Option<String>,
    /// The last dispatch sequence — carried in heartbeats and RESUME.
    seq: Option<u64>,
    /// Consecutive full IDENTIFYs without a READY/RESUMED since the last
    /// successful connection.
    identifies: u32,
}

/// A raw gateway frame: opcode, payload, sequence and event name.
#[derive(Debug, Deserialize)]
struct GatewayFrame {
    op: u8,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

/// The payload of a READY dispatch.
#[derive(Debug, Deserialize)]
struct ReadyPayload {
    session_id: String,
}

/// The payload of a MESSAGE_CREATE dispatch.
#[derive(Debug, Deserialize)]
struct MessageCreate {
    channel_id: String,
    #[serde(default)]
    content: String,
    author: User,
    /// Present on guild messages; `member.user` is the author in the guild.
    #[serde(default)]
    member: Option<Member>,
}

/// A Discord user.
#[derive(Debug, Deserialize)]
struct User {
    id: String,
    /// Bot accounts are skipped — they would echo the bot to itself.
    #[serde(default)]
    bot: bool,
}

/// A guild member, wrapping the same [`User`].
#[derive(Debug, Deserialize)]
struct Member {
    #[serde(default)]
    user: Option<User>,
}

/// The payload of an INTERACTION_CREATE dispatch.
#[derive(Debug, Deserialize)]
struct InteractionCreate {
    id: String,
    /// The interaction token — the callback URL's credential.
    token: String,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    data: Option<InteractionData>,
    /// The presser — present on DM interactions.
    #[serde(default)]
    user: Option<User>,
    /// The guild member who pressed; `member.user` is the presser in guilds.
    #[serde(default)]
    member: Option<Member>,
}

/// The interaction's component data.
#[derive(Debug, Deserialize)]
struct InteractionData {
    /// The button's `custom_id` — `approve:<call_id>` / `deny:<call_id>`.
    #[serde(default)]
    custom_id: Option<String>,
}

/// The Discord adapter: gateway receive loop plus REST outbound.
///
/// One instance serves a whole bot: `receive` runs the gateway connection
/// (with RESUME-first reconnects and a missed-heartbeat watchdog), while
/// `send_text` / `send_approval` / `edit_approval` and the interaction
/// callback talk to the REST API. All outbound calls tolerate one 429
/// retry. The token lives in this struct and is never logged or printed.
pub struct DiscordTransport {
    token: String,
    http: reqwest::Client,
    gateway_url: String,
    rest_base: String,
}

impl DiscordTransport {
    /// A transport pointed at Discord's production gateway and REST API.
    pub fn new(token: String) -> Self {
        Self::with_urls(token, GATEWAY_URL.to_string(), REST_BASE.to_string())
    }

    /// A transport with explicit gateway and REST endpoints.
    ///
    /// Tests point these at mock servers; self-hosted deployments use it
    /// to route through a proxy. The token is used verbatim either way.
    pub fn with_urls(token: String, gateway_url: String, rest_base: String) -> Self {
        Self { token, http: reqwest::Client::new(), gateway_url, rest_base }
    }
}

/// `Debug` redacts the token — the transport is never a place a secret
/// should leak through a log line.
impl std::fmt::Debug for DiscordTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordTransport")
            .field("gateway_url", &self.gateway_url)
            .field("rest_base", &self.rest_base)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl ChatTransport for DiscordTransport {
    async fn send_text(&self, chat: &ChatRef, text: &str) -> Result<(), ChatError> {
        let path = format!("/channels/{}/messages", chat.chat_id);
        let body = json!({ "content": truncate_content(text) });
        self.post_json(&path, body, true).await?;
        Ok(())
    }

    async fn send_approval(
        &self,
        chat: &ChatRef,
        request: &ApprovalRequest,
        approval_id: &str,
    ) -> Result<ApprovalMessage, ChatError> {
        let path = format!("/channels/{}/messages", chat.chat_id);
        use crate::transport::{preflight_line, session_line};
        let body = json!({
            "content": truncate_content(&format!(
                "{}Tool `{}` needs approval.\n{}Arguments: {}\nReasons: {}",
                session_line(request).map(|line| format!("{line}\n")).unwrap_or_default(),
                request.tool_name,
                preflight_line(request).map(|line| format!("{line}\n")).unwrap_or_default(),
                request.arguments,
                request.reasons.join("; ")
            )),
            "components": [{
                "type": 1,
                "components": [
                    { "type": 2, "style": 1, "custom_id": format!("approve:{approval_id}"), "label": "Approve" },
                    { "type": 2, "style": 4, "custom_id": format!("deny:{approval_id}"), "label": "Deny" },
                ],
            }],
        });
        let response = self.post_json(&path, body, true).await?;
        let body: Value = response
            .json()
            .await
            .map_err(|e| ChatError::Http(format!("discord response: {e}")))?;
        let message_id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ChatError::Discord("message response had no id".into()))?;
        Ok(ApprovalMessage { chat_id: chat.chat_id.clone(), message_id: message_id.to_string() })
    }

    async fn edit_approval(&self, msg: &ApprovalMessage, outcome: &str) -> Result<(), ChatError> {
        let path = format!("/channels/{}/messages/{}", msg.chat_id, msg.message_id);
        // `components: []` removes the buttons; the content records the
        // outcome (this gate is the message's single editor).
        let body = json!({ "content": truncate_content(outcome), "components": [] });
        self.patch_json(&path, body, true).await?;
        Ok(())
    }

    async fn receive(self: Arc<Self>, driver: Arc<ChatDriver>) -> Result<(), ChatError> {
        let state = Arc::new(Mutex::new(GatewayState::default()));
        loop {
            let outcome = self.gateway_connection(&driver, Arc::clone(&state)).await;
            match outcome {
                Err(ChatError::Fatal(message)) => return Err(ChatError::Fatal(message)),
                Err(_) => {
                    // A transient websocket failure — back off so a dead
                    // endpoint cannot spin.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Ok(()) => {
                    // A fresh IDENTIFY must wait out Discord's identify
                    // rate limit; a RESUME (the session is still valid)
                    // can reconnect at once.
                    if state.lock().await.session_id.is_none() {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }
}

impl DiscordTransport {
    /// One gateway connection: connect, HELLO, IDENTIFY or RESUME, then
    /// the read loop. `Ok` means "reconnect" — the outer loop decides
    /// whether to resume or identify; `Err(Fatal)` means give up.
    async fn gateway_connection(
        &self,
        driver: &Arc<ChatDriver>,
        state: Arc<Mutex<GatewayState>>,
    ) -> Result<(), ChatError> {
        let (ws, _response) = connect_async(&self.gateway_url)
            .await
            .map_err(|e| ChatError::Ws(format!("gateway connect: {e}")))?;
        let (mut write, mut read) = ws.split();

        // HELLO fixes the heartbeat interval.
        let hello = read_until_op(&mut read, OP_HELLO).await?;
        let interval = Duration::from_millis(
            hello.get("heartbeat_interval").and_then(Value::as_u64).unwrap_or(41_250),
        );

        // IDENTIFY or RESUME, depending on whether a session survived the
        // last connection.
        let resumed_attempted = {
            let session = state.lock().await;
            match session.session_id.clone() {
                Some(session_id) => {
                    let seq = session.seq;
                    write
                        .send(WsMessage::Text(
                            resume_payload(&self.token, &session_id, seq).to_string().into(),
                        ))
                        .await
                        .map_err(|e| ChatError::Ws(format!("gateway send: {e}")))?;
                    true
                }
                None => {
                    let mut session = session;
                    session.identifies += 1;
                    if session.identifies >= MAX_IDENTIFIES {
                        return Err(ChatError::Fatal(format!(
                            "Discord gateway: {MAX_IDENTIFIES} consecutive IDENTIFYs without \
                             a READY — is the token valid?"
                        )));
                    }
                    write
                        .send(WsMessage::Text(identify_payload(&self.token).to_string().into()))
                        .await
                        .map_err(|e| ChatError::Ws(format!("gateway send: {e}")))?;
                    false
                }
            }
        };

        // The heartbeat task owns the write half; the reader shares it (an
        // INVALID_SESSION re-identify has to send too).
        let write = Arc::new(Mutex::new(write));
        let ack = Arc::new(Notify::new());
        let heartbeat = {
            let write = Arc::clone(&write);
            let state = Arc::clone(&state);
            let ack = Arc::clone(&ack);
            tokio::spawn(async move { heartbeat_loop(write, state, ack, interval).await })
        };

        let outcome = async {
            let mut got_ready = false;
            loop {
                let frame = match read.next().await {
                    None => break Ok(()),
                    Some(Err(_)) => break Ok(()),
                    Some(Ok(WsMessage::Text(text))) => {
                        serde_json::from_str::<GatewayFrame>(text.as_str())
                            .map_err(|e| ChatError::Discord(format!("bad gateway frame: {e}")))?
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        let code = frame.as_ref().map(|f| f.code);
                        if code == Some(CloseCode::from(4004)) {
                            break Err(ChatError::Fatal(
                                "Discord gateway: authentication failed (close code 4004)"
                                    .into(),
                            ));
                        }
                        // Close 4009 means the session timed out — a
                        // RESUME will never succeed with it. The same is
                        // true whenever a RESUME attempt never heard
                        // READY/RESUMED back.
                        if code == Some(CloseCode::from(4009)) {
                            state.lock().await.session_id = None;
                        } else if resumed_attempted && !got_ready {
                            state.lock().await.session_id = None;
                        }
                        break Ok(());
                    }
                    Some(Ok(_)) => continue,
                };
                match frame.op {
                    OP_DISPATCH => {
                        if let Some(seq) = frame.s {
                            state.lock().await.seq = Some(seq);
                        }
                        match frame.t.as_deref() {
                            Some("READY") => {
                                let ready: ReadyPayload = serde_json::from_value(frame.d)
                                    .map_err(|e| ChatError::Discord(format!("bad READY: {e}")))?;
                                let mut session = state.lock().await;
                                session.session_id = Some(ready.session_id);
                                session.identifies = 0;
                                got_ready = true;
                            }
                            Some("RESUMED") => {
                                state.lock().await.identifies = 0;
                                got_ready = true;
                            }
                            Some("MESSAGE_CREATE") => {
                                let message: MessageCreate = serde_json::from_value(frame.d)
                                    .map_err(|e| {
                                        ChatError::Discord(format!("bad MESSAGE_CREATE: {e}"))
                                    })?;
                                if message.author.bot {
                                    continue;
                                }
                                let text = message.content;
                                if text.trim().is_empty() {
                                    // Attachment-only or otherwise
                                    // empty — nothing to run.
                                    continue;
                                }
                                // DMs carry `user` on the author; guild
                                // messages carry `member.user`.
                                let user_id = match message.member.and_then(|m| m.user) {
                                    Some(user) => user.id,
                                    None => message.author.id,
                                };
                                let incoming = IncomingMessage {
                                    chat: ChatRef {
                                        platform: "discord",
                                        chat_id: message.channel_id,
                                        user_id,
                                    },
                                    text,
                                };
                                driver.on_message(incoming.chat, incoming.text).await;
                            }
                            Some("INTERACTION_CREATE") => {
                                let interaction: InteractionCreate =
                                    serde_json::from_value(frame.d).map_err(|e| {
                                        ChatError::Discord(format!(
                                            "bad INTERACTION_CREATE: {e}"
                                        ))
                                    })?;
                                let Some(data) = interaction.data else { continue };
                                let Some(custom_id) = data.custom_id else { continue };
                                let Some((verb, approval_id)) = custom_id.split_once(':') else {
                                    continue;
                                };
                                let approved = match verb {
                                    "approve" => true,
                                    "deny" => false,
                                    _ => continue,
                                };
                                // The presser mirrors the message path: DMs
                                // carry `user`, guild messages
                                // `member.user`.
                                let user_id = match interaction.member.and_then(|m| m.user) {
                                    Some(user) => user.id,
                                    None => interaction.user.map(|u| u.id).unwrap_or_default(),
                                };
                                // Receiver-only acknowledgement: the
                                // interaction token in the URL is the
                                // credential — deliberately no
                                // Authorization header here.
                                let callback = format!(
                                    "/interactions/{}/{}/callback",
                                    interaction.id, interaction.token
                                );
                                let _ = self
                                    .post_json(&callback, json!({ "type": 6 }), false)
                                    .await;
                                let channel_id = interaction.channel_id.clone();
                                let press = ApprovalButtonPress {
                                    chat_id: interaction.channel_id,
                                    approval_id: approval_id.to_string(),
                                    approved,
                                    user_id,
                                };
                                let outcome = driver.on_approval(press).await;
                                if let PressOutcome::WrongUser = outcome {
                                    // The requester's buttons must stay — a
                                    // polite toast, never an edit of their
                                    // message.
                                    let _ = self
                                        .post_json(
                                            &format!("/channels/{channel_id}/messages"),
                                            json!({
                                                "content": crate::driver::WRONG_USER_TOAST
                                            }),
                                            true,
                                        )
                                        .await;
                                }
                            }
                            _ => {}
                        }
                    }
                    OP_HEARTBEAT => {
                        // The gateway asked for a heartbeat — answer.
                        let seq = state.lock().await.seq;
                        let frame = heartbeat_payload(seq);
                        if write
                            .lock()
                            .await
                            .send(WsMessage::Text(frame.to_string().into()))
                            .await
                            .is_err()
                        {
                            break Ok(());
                        }
                    }
                    OP_HEARTBEAT_ACK => ack.notify_one(),
                    OP_RECONNECT => break Ok(()),
                    OP_INVALID_SESSION => {
                        // The session is gone — drop it and reconnect; the
                        // outer loop's 5s backoff precedes the fresh
                        // IDENTIFY.
                        state.lock().await.session_id = None;
                        break Ok(());
                    }
                    _ => {}
                }
            }
        }
        .await;

        heartbeat.abort();
        outcome
    }

    /// POST JSON to `path`, 429-retried, returning the response. With
    /// `authorized` the request carries `Authorization: Bot {token}`.
    async fn post_json(
        &self,
        path: &str,
        body: Value,
        authorized: bool,
    ) -> Result<reqwest::Response, ChatError> {
        let url = self.url_for(path);
        let mut builder = self.http.post(&url).json(&body);
        if authorized {
            builder = builder.header("Authorization", format!("Bot {}", self.token));
        }
        let req = builder.build().map_err(|e| ChatError::Http(e.to_string()))?;
        let response = self.send_with_retry(req).await?;
        if !response.status().is_success() {
            return Err(ChatError::Discord(format!("POST {path} failed: {}", response.status())));
        }
        Ok(response)
    }

    /// PATCH JSON to `path`, 429-retried, returning the response.
    async fn patch_json(
        &self,
        path: &str,
        body: Value,
        authorized: bool,
    ) -> Result<reqwest::Response, ChatError> {
        let url = self.url_for(path);
        let mut builder = self.http.patch(&url).json(&body);
        if authorized {
            builder = builder.header("Authorization", format!("Bot {}", self.token));
        }
        let req = builder.build().map_err(|e| ChatError::Http(e.to_string()))?;
        let response = self.send_with_retry(req).await?;
        if !response.status().is_success() {
            return Err(ChatError::Discord(format!("PATCH {path} failed: {}", response.status())));
        }
        Ok(response)
    }

    /// Execute `req`, retrying once on HTTP 429 after sleeping
    /// `retry_after` seconds plus a half-second margin.
    async fn send_with_retry(
        &self,
        req: reqwest::Request,
    ) -> Result<reqwest::Response, ChatError> {
        let mut attempts = 0;
        loop {
            let next = req
                .try_clone()
                .ok_or_else(|| ChatError::Http("request body not re-sendable".into()))?;
            let response = self
                .http
                .execute(next)
                .await
                .map_err(|e| ChatError::Http(e.to_string()))?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempts < 1 {
                let retry_after = response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("retry_after").and_then(Value::as_f64))
                    .unwrap_or(0.0);
                tokio::time::sleep(Duration::from_secs_f64(retry_after + 0.5)).await;
                attempts += 1;
                continue;
            }
            return Ok(response);
        }
    }

    /// The full REST URL for a path (`path` starts with `/`).
    fn url_for(&self, path: &str) -> String {
        format!("{}{}", self.rest_base, path)
    }
}

/// Read frames until one with `op` arrives, returning its payload. A
/// [`HELLO_TIMEOUT`] cap turns a silent gateway into an error so the outer
/// loop reconnects.
async fn read_until_op(read: &mut WsRead, op: u8) -> Result<Value, ChatError> {
    tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match read.next().await {
                None => return Err(ChatError::Ws("gateway closed before HELLO".into())),
                Some(Err(e)) => return Err(ChatError::Ws(format!("gateway read: {e}"))),
                Some(Ok(WsMessage::Text(text))) => {
                    let frame: GatewayFrame = serde_json::from_str(text.as_str())
                        .map_err(|e| ChatError::Discord(format!("bad gateway frame: {e}")))?;
                    if frame.op == op {
                        return Ok(frame.d);
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .map_err(|_| ChatError::Ws(format!("gateway: no op {op} within {HELLO_TIMEOUT:?}")))?
}

/// The heartbeat task: send op 1 every `interval`, wait for the op 11 ack;
/// two missed acks in a row close the socket so the reader resumes.
async fn heartbeat_loop(
    write: Arc<Mutex<WsWrite>>,
    state: Arc<Mutex<GatewayState>>,
    ack: Arc<Notify>,
    interval: Duration,
) {
    let mut missed: u32 = 0;
    loop {
        {
            let seq = state.lock().await.seq;
            let frame = heartbeat_payload(seq);
            let mut write = write.lock().await;
            if write.send(WsMessage::Text(frame.to_string().into())).await.is_err() {
                return; // the socket is gone — the reader handles the reconnect
            }
        }
        tokio::select! {
            _ = ack.notified() => missed = 0,
            _ = tokio::time::sleep(interval) => missed += 1,
        }
        if missed >= 2 {
            // The gateway has stopped acknowledging — force a reconnect;
            // the reader sees the close and resumes the session.
            let _ = write.lock().await.send(WsMessage::Close(None)).await;
            return;
        }
    }
}

/// An op 1 heartbeat frame carrying the last dispatch sequence.
fn heartbeat_payload(seq: Option<u64>) -> Value {
    json!({ "op": OP_HEARTBEAT, "d": seq })
}

/// An op 2 identify frame with the token, intents and client properties.
fn identify_payload(token: &str) -> Value {
    json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": token,
            "intents": INTENTS,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "amparo",
                "device": "amparo",
            },
        },
    })
}

/// An op 6 resume frame with the token, session id and last sequence.
fn resume_payload(token: &str, session_id: &str, seq: Option<u64>) -> Value {
    json!({
        "op": OP_RESUME,
        "d": { "token": token, "session_id": session_id, "seq": seq },
    })
}

/// Truncate `s` to [`MAX_MESSAGE_CHARS`] characters — Discord's per-message
/// limit.
fn truncate_content(s: &str) -> String {
    s.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// Serve the Discord adapter until the receive loop ends or Ctrl-C arrives.
///
/// Reads the bot token from `AMPARO_CHAT_DISCORD_TOKEN` — missing is fatal
/// and fail-closed, the token never appears in output. The driver comes
/// from [`build_driver`], which assembles the shared inference config,
/// policy, registry and allowlist.
pub async fn serve(flags: &ChatFlags) -> Result<(), ChatError> {
    let token = std::env::var("AMPARO_CHAT_DISCORD_TOKEN")
        .map_err(|_| ChatError::Fatal("AMPARO_CHAT_DISCORD_TOKEN is required".into()))?;
    let transport: Arc<dyn ChatTransport> = Arc::new(DiscordTransport::new(token));
    let driver = build_driver(flags, Arc::clone(&transport))
        .await
        .map_err(|e| ChatError::Fatal(format!("{e}")))?;
    tokio::select! {
        result = transport.receive(driver) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_are_37376() {
        assert_eq!(INTENTS, 37376, "guild_messages | direct_messages | message_content");
    }

    #[test]
    fn heartbeat_payload_carries_op_and_seq() {
        assert_eq!(heartbeat_payload(Some(7)), json!({ "op": 1, "d": 7 }));
        assert_eq!(heartbeat_payload(None), json!({ "op": 1, "d": null }));
    }

    #[test]
    fn identify_payload_carries_token_intents_and_properties() {
        let payload = identify_payload("sekret");
        assert_eq!(payload["op"], 2);
        assert_eq!(payload["d"]["token"], "sekret");
        assert_eq!(payload["d"]["intents"], INTENTS);
        assert!(payload["d"]["properties"]["os"].is_string());
    }

    #[test]
    fn resume_payload_carries_session_and_seq() {
        let payload = resume_payload("sekret", "sess-1", Some(9));
        assert_eq!(payload["op"], 6);
        assert_eq!(payload["d"]["session_id"], "sess-1");
        assert_eq!(payload["d"]["seq"], 9);
    }

    #[test]
    fn text_is_truncated_to_discords_limit() {
        let long = "x".repeat(2500);
        assert_eq!(truncate_content(&long).chars().count(), MAX_MESSAGE_CHARS);
        assert_eq!(truncate_content("fine"), "fine");
    }

    #[test]
    fn debug_output_redacts_the_token() {
        let transport = DiscordTransport::with_urls(
            "super-secret-token".into(),
            "ws://mock".into(),
            "http://mock".into(),
        );
        let debug = format!("{transport:?}");
        assert!(!debug.contains("super-secret-token"), "Debug must not leak the token: {debug}");
    }
}
