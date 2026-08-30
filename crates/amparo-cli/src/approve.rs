//! Interactive approval: ask a human at the terminal.
//!
//! The loop's gate contract says an implementation must return within a
//! bounded time, so the gate reads stdin under a timeout and denies on
//! timeout, EOF or unreadable input — a piped `</dev/null` agent can never
//! hang waiting for a human. `--auto-approve`/`--auto-deny` never construct
//! this gate at all.

use amparo_agent::{ApprovalGate, ApprovalRequest};
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::Mutex;

/// How long to wait for a human answer before denying.
pub const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Re-prompts before giving up on unparseable input.
const MAX_PROMPTS: usize = 3;

/// Parse a yes/no answer: `Some(true)` for y/yes, `Some(false)` for n/no,
/// `None` for anything else (case- and whitespace-insensitive).
pub fn decide(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// The default approval gate: prints the request to stderr, reads one line
/// from stdin under [`APPROVAL_TIMEOUT`], and denies on anything but a clear
/// answer.
pub struct InteractiveApprovalGate {
    timeout: std::time::Duration,
    input: Mutex<Box<dyn AsyncBufRead + Unpin + Send>>,
}

impl InteractiveApprovalGate {
    /// The terminal gate: reads from the process stdin.
    pub fn new() -> Self {
        Self::with_reader(Box::new(tokio::io::BufReader::new(tokio::io::stdin())))
    }

    /// Build the gate over a specific reader (tests, and embedders piping
    /// a non-terminal input).
    pub fn with_reader(reader: Box<dyn AsyncBufRead + Unpin + Send>) -> Self {
        Self {
            timeout: APPROVAL_TIMEOUT,
            input: Mutex::new(reader),
        }
    }

    #[cfg(test)]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for InteractiveApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ApprovalGate for InteractiveApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        // stderr is unbuffered, so this lands before the stdin read.
        eprintln!();
        eprintln!("{}", prompt_text(request));

        let mut input = self.input.lock().await;
        for attempt in 1..=MAX_PROMPTS {
            eprint!("  approve? [y/N] ");
            let mut line = String::new();
            match tokio::time::timeout(self.timeout, input.read_line(&mut line)).await {
                Err(_) => {
                    eprintln!(
                        "[approval] no input in {}s — denying",
                        self.timeout.as_secs()
                    );
                    return false;
                }
                Ok(Err(e)) => {
                    eprintln!("[approval] input error ({e}) — denying");
                    return false;
                }
                Ok(Ok(0)) => {
                    eprintln!("[approval] no input (EOF) — denying");
                    return false;
                }
                Ok(Ok(_)) => match decide(&line) {
                    Some(approved) => return approved,
                    None if attempt < MAX_PROMPTS => {
                        eprintln!("[approval] please answer y or n");
                    }
                    None => {
                        eprintln!(
                            "[approval] no clear answer after {MAX_PROMPTS} prompts — denying"
                        );
                        return false;
                    }
                },
            }
        }
        false
    }
}

/// The prompt block printed to stderr before the y/n question: the tool
/// and its arguments, the M7 preflight blast-radius line (when the host
/// classified the call — the human approves a concrete consequence, not
/// an abstraction), and the gate reasons.
fn prompt_text(request: &ApprovalRequest) -> String {
    let mut lines = Vec::new();
    // M8: a sub-agent's ask is labeled with its delegation chain — who
    // is asking, before what they want to run. Display-only (I1).
    if let Some(label) = &request.session_label {
        lines.push(format!("[session] {label} wants to run:"));
    }
    lines.push(format!(
        "[approval] {} {}",
        request.tool_name,
        serde_json::to_string_pretty(&request.arguments).unwrap_or_default()
    ));
    if let Some(radius) = &request.blast_radius {
        lines.push(format!(
            "[preflight] blast radius: {radius} — {}",
            radius.note()
        ));
    }
    for reason in &request.reasons {
        lines.push(format!("  because: {reason}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_agent::BlastRadius;
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "c1".into(),
            tool_name: "run_command".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
            reasons: vec!["reaches outside the process".into()],
            blast_radius: Some(BlastRadius::Network),
            session_label: None,
        }
    }

    fn gate(lines: &'static str) -> InteractiveApprovalGate {
        InteractiveApprovalGate::with_reader(Box::new(Cursor::new(lines.as_bytes().to_vec())))
    }

    #[test]
    fn decide_parses_the_yes_no_surface() {
        assert_eq!(decide("y"), Some(true));
        assert_eq!(decide("YES"), Some(true));
        assert_eq!(decide(" yes \n"), Some(true));
        assert_eq!(decide("n"), Some(false));
        assert_eq!(decide("No"), Some(false));
        assert_eq!(decide(""), None);
        assert_eq!(decide("maybe"), None);
        assert_eq!(decide("yep"), None);
    }

    #[tokio::test]
    async fn grants_on_yes_and_denies_on_no() {
        assert!(gate("y\n").request(&request()).await);
        assert!(gate("yes\n").request(&request()).await);
        assert!(!gate("n\n").request(&request()).await);
        assert!(!gate("no\n").request(&request()).await);
    }

    #[test]
    fn prompt_text_puts_the_blast_radius_above_the_reasons() {
        let text = prompt_text(&request());
        let preflight = text.find("[preflight] blast radius: network");
        let because = text.find("  because:");
        let Some((preflight, because)) = preflight.zip(because) else {
            panic!("expected both lines, got: {text}");
        };
        assert!(preflight < because, "preflight must come first: {text}");
    }

    #[test]
    fn no_classification_omits_the_preflight_line() {
        let mut request = request();
        request.blast_radius = None;
        assert!(
            !prompt_text(&request).contains("[preflight]"),
            "{}",
            prompt_text(&request)
        );
    }

    #[test]
    fn session_label_leads_the_copy_for_a_sub_agent() {
        let mut request = request();
        request.session_label = Some("sub-agent sess-123.1 of task sess-123".into());
        let text = prompt_text(&request);
        let session = text.find("[session] sub-agent sess-123.1 of task sess-123 wants to run:");
        let approval = text.find("[approval]");
        let Some((session, approval)) = session.zip(approval) else {
            panic!("expected both lines, got: {text}");
        };
        assert!(
            session < approval,
            "the session label leads the copy: {text}"
        );
    }

    #[test]
    fn no_session_label_omits_the_session_line() {
        assert!(
            !prompt_text(&request()).contains("[session]"),
            "{}",
            prompt_text(&request())
        );
    }

    #[tokio::test]
    async fn eof_denies_instead_of_hanging() {
        assert!(!gate("").request(&request()).await);
    }

    #[tokio::test]
    async fn unparseable_input_re_prompts_then_grants() {
        assert!(gate("maybe\nwhatever\ny\n").request(&request()).await);
    }

    #[tokio::test]
    async fn unparseable_input_exhausts_prompts_then_denies() {
        assert!(!gate("maybe\nz\nx\n").request(&request()).await);
    }

    /// A reader that never produces data — the timeout must fire, not the
    /// EOF path.
    struct Stalled;

    impl tokio::io::AsyncRead for Stalled {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncBufRead for Stalled {
        fn poll_fill_buf(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<&[u8]>> {
            Poll::Pending
        }

        fn consume(self: Pin<&mut Self>, _amt: usize) {}
    }

    #[tokio::test]
    async fn stalled_input_times_out_and_denies() {
        let gate = InteractiveApprovalGate::with_reader(Box::new(Stalled))
            .with_timeout(std::time::Duration::from_millis(20));
        assert!(!gate.request(&request()).await);
    }
}
