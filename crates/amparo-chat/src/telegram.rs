//! The Telegram adapter — `getUpdates` long polling with inline-keyboard
//! approval.
//!
//! [`TelegramTransport`] is a dumb, reliable [`ChatTransport`]: outbound
//! calls are form POSTs to the Bot API (`{base}/bot{token}/{method}`), and
//! [`receive`](TelegramTransport::receive) polls `getUpdates` with a
//! 50-second long poll, feeding normalized messages and button presses to
//! the driver. The bot token is folded into the API base and never appears
//! in any log line (see `TelegramTransport::redact` for the one place a
//! transport error could leak it).
//!
//! Fail-closed: a `getUpdates` rejection with error code 401 or 409 — a bad
//! or concurrently-used token — is [`ChatError::Fatal`], so the serve loop
//! exits instead of silently starving; every other failure is logged and
//! retried after 5 seconds. [`serve`] checks the token, wires the shared
//! driver assembly ([`crate::dispatch::build_driver`]), and runs the loop
//! until Ctrl-C or a fatal error. The transport is rooted at the Bot API
//! base from `AMPARO_CHAT_TELEGRAM_BASE` (default
//! `https://api.telegram.org`), so tests and self-hosted proxies can point
//! [`serve`] at their own endpoint.

use crate::dispatch::{build_driver, ChatFlags, ChatServeError};
use crate::driver::ChatDriver;
use crate::transport::{
    ApprovalButtonPress, ApprovalMessage, ChatError, ChatRef, ChatTransport, PressOutcome,
};
use amparo_agent::ApprovalRequest;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// How long a single Bot API call may take. Must exceed the 50-second
/// `getUpdates` long poll, or a quiet poll would be cut short by the client
/// timeout instead of the server's.
const HTTP_TIMEOUT: Duration = Duration::from_secs(65);

/// How long a `getUpdates` poll asks the server to wait, in seconds.
const LONG_POLL_SECS: u64 = 50;

/// The maximum text length `sendMessage` accepts; longer text is truncated
/// at a UTF-8 boundary by [`truncate`].
const MAX_TEXT_CHARS: usize = 4000;

/// The Telegram transport: outbound Bot API calls plus the long-poll
/// receive loop.
///
/// `base` is the API root the constructor folds the token into
/// (`{base}/bot{token}`), so every call is `POST {base}/{method}` — tests
/// point `base` at a local mock server. The client timeout is
/// `HTTP_TIMEOUT`, so a quiet 50-second long poll is never cut short by
/// the client.
pub struct TelegramTransport {
    /// The API base including the folded-in bot token. Never logged.
    base: String,
    /// The HTTP client used for every Bot API call.
    client: reqwest::Client,
}

impl TelegramTransport {
    /// Build a transport rooted at `base` (the real API —
    /// `https://api.telegram.org` — or a mock in tests) with bot `token`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        let base = format!("{}/bot{}", base.into().trim_end_matches('/'), token.into());
        Self {
            base,
            client: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("static reqwest client options are valid"),
        }
    }

    /// POST `{method}` with form-encoded `params` and return the raw JSON
    /// response body. Callers check the `ok` flag via [`checked`].
    async fn api_post<S: serde::Serialize>(
        &self,
        method: &str,
        params: &S,
    ) -> Result<Value, ChatError> {
        let response = self
            .client
            .post(format!("{}/{}", self.base, method))
            .form(params)
            .send()
            .await
            .map_err(|e| ChatError::Telegram(format!("{method}: {e}")))?;
        response
            .json()
            .await
            .map_err(|e| ChatError::Telegram(format!("{method}: response was not JSON: {e}")))
    }

    /// Split an API response into the `result` value, or an error carrying
    /// the API's `description`.
    fn checked(method: &str, body: Value) -> Result<Value, ChatError> {
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(body.get("result").cloned().unwrap_or(Value::Null))
        } else {
            let description = body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("no description");
            Err(ChatError::Telegram(format!(
                "{method}: API error: {description}"
            )))
        }
    }

    /// A log-safe rendering of a transport error: reqwest error text can
    /// embed the request URL, which carries the bot token — the base is
    /// replaced so the token never reaches a log line.
    fn redact(&self, error: &ChatError) -> String {
        format!("{error}").replace(&self.base, "<telegram api>")
    }

    /// Answer one callback query so the client's spinner stops.
    pub(crate) async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<(), ChatError> {
        let params = [("callback_query_id", callback_id), ("text", text)];
        let result = self.api_post("answerCallbackQuery", &params).await?;
        let _ = Self::checked("answerCallbackQuery", result)?;
        Ok(())
    }

    /// Handle one inbound message: skip anything without a sender or with
    /// empty text, then forward a normalized [`ChatRef`] to the driver.
    async fn handle_message(&self, message: &Message, driver: &ChatDriver) {
        let Some(from) = &message.from else { return };
        let Some(text) = &message.text else { return };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let chat = ChatRef {
            platform: "telegram",
            chat_id: message.chat.id.to_string(),
            user_id: from.id.to_string(),
        };
        driver.on_message(chat, text.to_string()).await;
    }

    /// Handle one callback query: parse the button payload, route the press
    /// to the driver, and always answer with a short toast so the client's
    /// spinner stops.
    async fn handle_callback(&self, query: &CallbackQuery, driver: &ChatDriver) {
        let toast = match parse_button(&query.data) {
            Some((approval_id, approved)) => {
                let chat_id = query
                    .message
                    .as_ref()
                    .map(|message| message.chat.id.to_string())
                    .unwrap_or_else(|| query.from.id.to_string());
                let press = ApprovalButtonPress {
                    chat_id,
                    approval_id,
                    approved,
                    user_id: query.from.id.to_string(),
                };
                match driver.on_approval(press).await {
                    PressOutcome::Routed => {
                        if approved {
                            "Approved"
                        } else {
                            "Denied"
                        }
                    }
                    PressOutcome::AlreadyDecided => "Already decided — no longer pending",
                    PressOutcome::WrongUser => crate::driver::WRONG_USER_TOAST,
                }
            }
            None => "Unknown action",
        };
        let _ = self.answer_callback(&query.id, toast).await;
    }
}

#[async_trait]
impl ChatTransport for TelegramTransport {
    async fn send_text(&self, chat: &ChatRef, text: &str) -> Result<(), ChatError> {
        let text = truncate(text);
        let params = [("chat_id", chat.chat_id.as_str()), ("text", text.as_str())];
        let result = self.api_post("sendMessage", &params).await?;
        let _ = Self::checked("sendMessage", result)?;
        Ok(())
    }

    async fn send_approval(
        &self,
        chat: &ChatRef,
        request: &ApprovalRequest,
        approval_id: &str,
    ) -> Result<ApprovalMessage, ChatError> {
        // Inline Approve/Deny buttons carrying the approval id — the tool
        // call id, unique per task.
        let keyboard = json!({
            "inline_keyboard": [[
                {"text": "Approve", "callback_data": format!("approve:{approval_id}")},
                {"text": "Deny", "callback_data": format!("deny:{approval_id}")},
            ]]
        });
        use crate::transport::{preflight_line, rollback_line, session_line};
        let text = truncate(&format!(
            "{}Approval needed — {}: {}\n{}{}{}",
            session_line(request)
                .map(|line| format!("{line}\n"))
                .unwrap_or_default(),
            request.tool_name,
            request.arguments,
            preflight_line(request)
                .map(|line| format!("{line}\n"))
                .unwrap_or_default(),
            rollback_line(request)
                .map(|line| format!("{line}\n"))
                .unwrap_or_default(),
            request.reasons.join("; ")
        ));
        let keyboard = keyboard.to_string();
        let params = [
            ("chat_id", chat.chat_id.as_str()),
            ("text", text.as_str()),
            ("reply_markup", keyboard.as_str()),
        ];
        let result = self.api_post("sendMessage", &params).await?;
        let result = Self::checked("sendMessage", result)?;
        let message_id = result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ChatError::Telegram("sendMessage: result missing message_id".into()))?
            .to_string();
        Ok(ApprovalMessage {
            chat_id: chat.chat_id.clone(),
            message_id,
        })
    }

    async fn edit_approval(&self, msg: &ApprovalMessage, outcome: &str) -> Result<(), ChatError> {
        // An empty inline_keyboard removes the buttons.
        let params = [
            ("chat_id", msg.chat_id.as_str()),
            ("message_id", msg.message_id.as_str()),
            ("text", outcome),
            ("reply_markup", r#"{"inline_keyboard": []}"#),
        ];
        let result = self.api_post("editMessageText", &params).await?;
        let _ = Self::checked("editMessageText", result)?;
        Ok(())
    }

    async fn receive(self: Arc<Self>, driver: Arc<ChatDriver>) -> Result<(), ChatError> {
        let this = Arc::clone(&self);
        self.pump(move |update| {
            let this = Arc::clone(&this);
            let driver = Arc::clone(&driver);
            async move {
                if let Some(message) = &update.message {
                    this.handle_message(message, &driver).await;
                }
                if let Some(query) = &update.callback_query {
                    this.handle_callback(query, &driver).await;
                }
            }
        })
        .await
    }
}

impl TelegramTransport {
    /// The `getUpdates` long-poll loop shared by the driver mode and the
    /// R3b receiver: `handle` is called with every confirmed update, in
    /// order. The loop exits on Ctrl-C or a fatal token rejection (401/409)
    /// and retries everything else after 5 seconds.
    pub(crate) async fn pump<Fut>(
        self: Arc<Self>,
        mut handle: impl FnMut(Update) -> Fut,
    ) -> Result<(), ChatError>
    where
        Fut: std::future::Future<Output = ()> + Send,
    {
        let mut offset: Option<i64> = None;
        loop {
            let mut params: Vec<(String, String)> = vec![
                ("limit".to_string(), "100".to_string()),
                ("timeout".to_string(), LONG_POLL_SECS.to_string()),
                (
                    "allowed_updates".to_string(),
                    r#"["message","callback_query"]"#.to_string(),
                ),
            ];
            if let Some(offset) = offset {
                params.push(("offset".to_string(), offset.to_string()));
            }

            // A quiet long poll lasts 50 seconds; the Ctrl-C arm keeps it
            // interruptible (serve() selects on Ctrl-C too, so either
            // completes the loop).
            let body = tokio::select! {
                _ = tokio::signal::ctrl_c() => return Ok(()),
                result = self.api_post("getUpdates", &params) => result,
            };
            let body = match body {
                Ok(body) => body,
                Err(error) => {
                    eprintln!(
                        "amparo chat telegram: getUpdates request failed: {}; retrying in 5s",
                        self.redact(&error)
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            let response: GetUpdatesResponse = match serde_json::from_value(body) {
                Ok(response) => response,
                Err(error) => {
                    eprintln!(
                        "amparo chat telegram: getUpdates response undecodable: {error}; \
                         retrying in 5s"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if !response.ok {
                let code = response.error_code;
                let description = response
                    .description
                    .unwrap_or_else(|| "no description".to_string());
                if code == Some(401) || code == Some(409) {
                    // A rejected token — bad credentials or a second process
                    // holding the same bot — is not retryable: exit rather
                    // than poll forever.
                    return Err(ChatError::Fatal(format!(
                        "getUpdates rejected (error_code {code:?}): {description}"
                    )));
                }
                eprintln!(
                    "amparo chat telegram: getUpdates failed ({code:?}): {description}; \
                     retrying in 5s"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            for update in response.result {
                // Confirmed updates are never re-delivered: the next poll
                // starts one past the last id we saw.
                offset = Some(update.update_id + 1);
                handle(update).await;
            }
        }
    }
}

/// One `getUpdates` response — the same shape covers both the success
/// (`result`) and error (`description`, `error_code`) forms.
#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
}

/// One update: a message, a callback query, or (rarely) both.
///
/// `pub(crate)` so the R3b receiver can reuse [`TelegramTransport::pump`]
/// and the wire structs with it.
#[derive(Debug, Deserialize)]
pub(crate) struct Update {
    /// The Telegram update id — the pump's offset cursor; a handler
    /// never needs it.
    #[allow(dead_code)]
    update_id: i64,
    #[serde(default)]
    pub(crate) message: Option<Message>,
    #[serde(default)]
    pub(crate) callback_query: Option<CallbackQuery>,
}

/// A chat message. `from` is absent for channel posts and `text` for
/// non-text messages — both are skipped by the receive loop.
#[derive(Debug, Deserialize)]
pub(crate) struct Message {
    /// Kept for wire fidelity; inbound message ids are not used by the
    /// receive loop (outbound ids come from sendMessage responses).
    #[allow(dead_code)]
    message_id: i64,
    #[serde(default)]
    from: Option<User>,
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
}

/// A chat — `id` is the chat identifier used everywhere outbound.
#[derive(Debug, Deserialize)]
pub(crate) struct Chat {
    id: i64,
}

/// A Telegram user — `id` becomes the `user_id` of a [`ChatRef`].
#[derive(Debug, Deserialize)]
pub(crate) struct User {
    pub(crate) id: i64,
}

/// A callback query — the wire form of an inline-button press.
#[derive(Debug, Deserialize)]
pub(crate) struct CallbackQuery {
    pub(crate) id: String,
    pub(crate) from: User,
    #[serde(default)]
    pub(crate) message: Option<Message>,
    #[serde(default)]
    pub(crate) data: Option<String>,
}

/// Parse an inline-button payload (`approve:<id>` / `deny:<id>`) into the
/// approval id and the decision. Anything else is not one of our buttons.
/// Shared with the R3b receiver, whose buttons carry the hub call_id in
/// the same grammar.
pub(crate) fn parse_button(data: &Option<String>) -> Option<(String, bool)> {
    let data = data.as_deref()?;
    let (action, id) = data.split_once(':')?;
    match action {
        "approve" => Some((id.to_string(), true)),
        "deny" => Some((id.to_string(), false)),
        _ => None,
    }
}

/// Truncate `text` to at most [`MAX_TEXT_CHARS`] bytes at a UTF-8
/// boundary, appending an ellipsis when anything was cut (the ellipsis's
/// bytes are reserved up front, so the result never exceeds the limit).
fn truncate(text: &str) -> String {
    if text.len() <= MAX_TEXT_CHARS {
        return text.to_string();
    }
    let mut end = MAX_TEXT_CHARS - "…".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The default Bot API root, used when `AMPARO_CHAT_TELEGRAM_BASE` is unset.
const DEFAULT_TELEGRAM_BASE: &str = "https://api.telegram.org";

/// The Bot API base [`serve`] roots the transport at: the value of
/// `AMPARO_CHAT_TELEGRAM_BASE`, or [`DEFAULT_TELEGRAM_BASE`] when unset.
pub(crate) fn telegram_base() -> String {
    std::env::var("AMPARO_CHAT_TELEGRAM_BASE").unwrap_or_else(|_| DEFAULT_TELEGRAM_BASE.to_string())
}

/// Serve the Telegram adapter: token fail-closed (exit 2), the shared
/// driver assembly, then the long-poll loop until Ctrl-C or a fatal error
/// (exit 1). The transport is rooted at the Bot API base from
/// `AMPARO_CHAT_TELEGRAM_BASE` (default `https://api.telegram.org`) — an
/// override for tests and self-hosted proxies.
pub async fn serve(flags: &ChatFlags) -> Result<(), ChatServeError> {
    let token = std::env::var("AMPARO_CHAT_TELEGRAM_TOKEN").map_err(|_| {
        ChatServeError::new(
            "AMPARO_CHAT_TELEGRAM_TOKEN is required — see the README chat section",
            2,
        )
    })?;
    let transport: Arc<dyn ChatTransport> =
        Arc::new(TelegramTransport::new(telegram_base(), token));
    let driver = build_driver(flags, transport.clone()).await?;
    tokio::select! {
        result = transport.receive(driver) => {
            result.map_err(|error| ChatServeError::new(error.to_string(), 1))
        }
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_untouched() {
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn truncate_reserves_room_for_the_ellipsis() {
        // Multibyte content: the cut must land on a char boundary and the
        // result (ellipsis included) must never exceed the limit.
        let long = "é".repeat(3000);
        let cut = truncate(&long);
        assert!(cut.len() <= MAX_TEXT_CHARS, "cut to {len}", len = cut.len());
        assert!(cut.ends_with('…'));
        let body = cut.trim_end_matches('…');
        assert!(
            body.is_char_boundary(body.len()),
            "the cut lands on a char boundary"
        );
        assert_eq!(
            body,
            &"é".repeat(body.chars().count()),
            "no partial characters"
        );
    }

    #[test]
    fn parse_button_decodes_approve_deny_and_junk() {
        assert_eq!(
            parse_button(&Some("approve:call_1".to_string())),
            Some(("call_1".to_string(), true))
        );
        assert_eq!(
            parse_button(&Some("deny:call_9".to_string())),
            Some(("call_9".to_string(), false))
        );
        assert_eq!(parse_button(&Some("nonsense".to_string())), None);
        assert_eq!(parse_button(&None), None);
    }

    #[test]
    fn telegram_base_env_overrides_and_falls_back() {
        std::env::remove_var("AMPARO_CHAT_TELEGRAM_BASE");
        assert_eq!(telegram_base(), DEFAULT_TELEGRAM_BASE);
        std::env::set_var("AMPARO_CHAT_TELEGRAM_BASE", "http://127.0.0.1:9999");
        assert_eq!(telegram_base(), "http://127.0.0.1:9999");
        std::env::remove_var("AMPARO_CHAT_TELEGRAM_BASE");
    }
}
