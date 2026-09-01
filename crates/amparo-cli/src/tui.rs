//! `amparo tui` — the interactive terminal surface.
//!
//! One prompt, the whole gate chain rendered live. The chain is the spine:
//! every tool call is one `◆▲§◉` row (registry → trust ceiling → policy →
//! human), dim while it waits, all green when it completes, colored at the
//! link that stopped it. Approvals render as a full card — the reasons, the
//! blast radius, the rollback — with a draining 60-second meter and
//! single-keypress `y`/`N`; deny on timeout, fail closed. No scroll regions,
//! no alternate screen: everything prints inline and scrolls like a log.
//!
//! The prompt is the input line: `› Amparo█` at rest (the block glyph marks
//! where the real cursor lives), ↑↓ history, bracketed paste, Ctrl-C cancels
//! a running task (twice to exit). A dim status register rides the right
//! edge when the terminal is wide enough — `◆▲§◉ · enforce · engram ·
//! ~$0.04` — and deviations recolor it live (`§` amber once the wire engine
//! answers in audit mode). `/policy` and `/memory` open ruled reports on the
//! companion consoles; `/resume` opens the checkpoint picker.
//!
//! **Degraded shapes** (all documented, all honest):
//! - *Piped* (`echo task | amparo tui`): one task per stdin line, zero
//!   escapes; approvals print the card and fail closed — no terminal to ask.
//! - *NO_COLOR / TERM=dumb*: the same surface without color or cursor
//!   control.
//! - *Windows*: line-mode — no raw mode, so no single-keypress approvals;
//!   approvals fail closed after printing the card.
//!
//! **Known simplifications, kept deliberate**: sub-agent calls render as
//! flat rows (tool events carry no task id); the session cost register is an
//! estimate (chars/4) topped up by each task's report; the `[memory]`
//! degrade notice prints on stderr through the tools layer, not this
//! surface; the final answer never prints on stdout — it arrives as the
//! `[answer]` event. Scientific voice, `[tag]` lines, `—` in copy, no
//! emoji.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use amparo_agent::{
    estimate_tokens, format_cost_line, format_event, AgentEvent, AgentReport, ApprovalGate,
    ApprovalRequest, Checkpoint, CheckpointStore, EventSink, JsonCheckpointStore, TaskStatus,
};
use amparo_policy::PolicyEngine;
use amparo_tools::{Memory, PathPolicy};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};

use crate::run::{self, BannerInfo, RunFlags, Surface, WiredRun};

/// How long an approval waits for a key before it denies itself.
const APPROVAL_SECS: u64 = 60;

/// A `Running` checkpoint older than this is not offered for resume (it
/// mirrors the staleness cut in `amparo run --resume`).
const STALE_CHECKPOINT_SECS: u64 = 7 * 24 * 60 * 60;

// ────────────────────────────────────────────────────────────── Paint ──

/// Color application for the whole surface. `colors: false` renders every
/// call as plain text — the piped / `NO_COLOR` / `TERM=dumb` contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Paint {
    colors: bool,
}

/// The palette inks. SGR 2 dim, SGR 3 italic — used exactly where the spec
/// says: italic only for estimates, bold only for emphasis.
#[derive(Clone, Copy)]
enum Ink {
    Fg,
    Dim,
    Accent,
    Ok,
    Bad,
    Warn,
    WarnBold,
    Bold,
    ItalicDim,
}

impl Paint {
    /// Builds a paint with colors on or off (the test seam).
    fn with_colors(colors: bool) -> Self {
        Self { colors }
    }

    /// Detects the surface: colors only when we control a terminal and
    /// `NO_COLOR` is absent.
    fn detect(controls: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self::with_colors(controls && !no_color)
    }

    /// Applies `ink` to `text` — identity when colors are off.
    fn apply(&self, ink: Ink, text: &str) -> String {
        if !self.colors {
            return text.to_string();
        }
        let sgr = match ink {
            Ink::Fg => "38;2;226;232;240",
            Ink::Dim => "2;38;2;148;163;184",
            Ink::Accent => "38;2;59;130;246",
            Ink::Ok => "38;2;16;185;129",
            Ink::Bad => "38;2;239;68;68",
            Ink::Warn => "38;2;245;158;11",
            Ink::WarnBold => "1;38;2;245;158;11",
            Ink::Bold => "1;38;2;226;232;240",
            Ink::ItalicDim => "3;2;38;2;148;163;184",
        };
        format!("\x1b[{sgr}m{text}\x1b[0m")
    }
}

// ──────────────────────────────────────────────────────────── the chain ──

/// One link of the gate chain, as a color state: green passed / amber
/// escalated-or-pending / red denied / dim never evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LinkState {
    Pass,
    Escalate,
    Pending,
    Denied,
    Unreached,
}

/// Renders the four chain symbols. Compact (`◆▲§◉`) for gutter rows,
/// expanded (`◆──▲──§──◉`) inside the approval card only.
fn render_chain(states: [LinkState; 4], expanded: bool, paint: &Paint) -> String {
    const SYMBOLS: [char; 4] = ['◆', '▲', '§', '◉'];
    let mut out = String::new();
    for (i, state) in states.iter().enumerate() {
        if i > 0 && expanded {
            out.push_str("──");
        }
        let ink = match state {
            LinkState::Pass => Ink::Ok,
            LinkState::Escalate => Ink::Warn,
            LinkState::Pending => Ink::WarnBold,
            LinkState::Denied => Ink::Bad,
            LinkState::Unreached => Ink::Dim,
        };
        out.push_str(&paint.apply(ink, &SYMBOLS[i].to_string()));
    }
    out
}

/// Maps a gate decision to the row that announces it — `None` when the
/// decision prints nothing (allowed calls complete as green rows) or when a
/// future decision should fall through to the plain `[gate]` line.
fn gate_states(decision: &str, escalated: bool) -> Option<[LinkState; 4]> {
    use LinkState::*;
    match decision {
        "trust_blocked" => Some([Pass, Denied, Unreached, Unreached]),
        "policy_denied" => Some([Pass, Pass, Denied, Unreached]),
        "approval_denied" => Some([Pass, Pass, if escalated { Escalate } else { Pass }, Denied]),
        "unknown_tool" => Some([Denied, Unreached, Unreached, Unreached]),
        "allowed" => None,
        _ => None,
    }
}

/// The short suffix a denied row carries after the tool description.
fn gate_suffix(decision: &str, reasons: &[String]) -> String {
    match decision {
        "trust_blocked" => "trust ceiling".to_string(),
        "policy_denied" => format!("policy: {}", clip(&reasons.join("; "), 40)),
        "approval_denied" => "you denied".to_string(),
        "unknown_tool" => "not registered".to_string(),
        _ => clip(&reasons.join("; "), 40),
    }
}

/// The `[key]` legend — short on the banner, full under `/key`.
fn key_line(paint: &Paint, full: bool) -> String {
    let mut s = format!(
        "[key]   {} · {} · {} · {}",
        paint.apply(Ink::Ok, "pass"),
        paint.apply(Ink::Warn, "escalate"),
        paint.apply(Ink::Bad, "denied"),
        paint.apply(Ink::Dim, "not reached")
    );
    if full {
        s.push_str(&paint.apply(Ink::Dim, " ── the symbols light up as each verdict arrives"));
    }
    s
}

/// Clips `s` to `max` chars, appending `…` when cut — the same convention
/// as the agent's `truncate`, at a column the caller chooses.
fn clip(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Display length of a string with SGR escapes stripped (the prompt's
/// right-alignment math).
fn plain_len(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                // CSI: ESC [ params … final byte 0x40..=0x7e.
                Some('[') => {
                    for d in chars.by_ref() {
                        if (d as u32) >= 0x40 && (d as u32) <= 0x7e {
                            break;
                        }
                    }
                }
                // Two-byte sequences (ESC c) — the second char is the body.
                Some(_) | None => {}
            }
        } else {
            n += 1;
        }
    }
    n
}

// ───────────────────────────────────────────────────────────── banner ──

/// The boot banner: the chain as an ASCII diagram, then the legend and the
/// live facts. Printed once per process; per-task wires suppress it.
fn banner_lines(info: &BannerInfo, paint: &Paint) -> Vec<String> {
    let sym = |c: &str| paint.apply(Ink::Accent, c);
    vec![
        "Greetings! My name is Amparo, built by EL AI Intelligence.".to_string(),
        "[wake] Amparo is awake.".to_string(),
        String::new(),
        "Every action I take passes one chain. No action exits it.".to_string(),
        String::new(),
        format!(
            "   {} registry  ──▶  {} trust ceiling  ──▶  {} policy  ──▶  {} you",
            sym("◆"),
            sym("▲"),
            sym("§"),
            paint.apply(Ink::Bold, "◉")
        ),
        String::new(),
        format!(
            "   {}  the tool must be registered, or it does not exist",
            sym("◆")
        ),
        format!(
            "   {}  its tier must sit under your trust ceiling",
            sym("▲")
        ),
        format!(
            "   {}  the policy engine must allow this exact call",
            sym("§")
        ),
        format!(
            "   {}  you approve whatever the first three cannot settle",
            paint.apply(Ink::Bold, "◉")
        ),
        String::new(),
        "Deny by default. Fail closed at every link. A model is never the approver.".to_string(),
        String::new(),
        key_line(paint, false),
        format!("[chain] {}", chain_line(info, false, paint)),
        format!("[infer] {}", info.infer),
        format!("[memory] {}", info.memory),
    ]
}

/// The `[chain]` line — the chain facts in one row. Re-printed mid-stream
/// the first time the policy engine answers in audit mode.
fn chain_line(info: &BannerInfo, audit: bool, paint: &Paint) -> String {
    format!(
        "{} registry: {} tools  ·  {} ceiling: {}  ·  {} policy: {}  ·  {} human: {}",
        paint.apply(Ink::Accent, "◆"),
        info.tool_count,
        paint.apply(Ink::Accent, "▲"),
        info.ceiling,
        paint.apply(Ink::Accent, "§"),
        policy_segment(&info.policy, audit, paint),
        paint.apply(Ink::Bold, "◉"),
        info.approval
    )
}

/// The policy register, mapped to honest names: a wire engine shows its
/// site and mode; the built-ins are named for what they are.
fn policy_segment(desc: &str, audit: bool, paint: &Paint) -> String {
    if audit {
        paint.apply(Ink::Warn, "audit — verdicts advisory")
    } else if desc.starts_with("wire ") {
        format!("{desc} (enforce)")
    } else if desc.starts_with("deny-all") {
        "built-in (deny-all default)".to_string()
    } else {
        "built-in (allow-all — operator opt-in)".to_string()
    }
}

// ─────────────────────────────────────────────────────── status register ──

/// The right-edge status line: chain states, policy segment (amber on
/// audit), memory backend, running cost estimate. `None` below 72 columns.
fn status_line(
    paint: &Paint,
    width: usize,
    audit: bool,
    policy_seg: &str,
    memory_seg: &str,
    cost: Option<f64>,
) -> Option<String> {
    if width < 72 {
        return None;
    }
    let chain = render_chain(
        [
            LinkState::Pass,
            LinkState::Pass,
            if audit {
                LinkState::Escalate
            } else {
                LinkState::Pass
            },
            LinkState::Pass,
        ],
        false,
        paint,
    );
    let policy = if audit {
        paint.apply(Ink::Warn, policy_seg)
    } else {
        paint.apply(Ink::Dim, policy_seg)
    };
    let mut segments = vec![policy, paint.apply(Ink::Dim, memory_seg)];
    if let Some(cost) = cost {
        segments.push(paint.apply(Ink::Dim, &format!("~${cost:.2}")));
    }
    Some(format!("{chain} · {}", segments.join(" · ")))
}

// ────────────────────────────────────────────────────────────── gutter ──

/// The argument column of a gutter row: a single-key object renders as its
/// value, everything else as compact JSON.
fn display_arg(arguments: &Value) -> String {
    match arguments {
        Value::Object(map) if map.len() == 1 => {
            let (_, v) = map.iter().next().expect("len 1");
            match v {
                Value::String(s) => clip(s, 60),
                other => serde_json::to_string(other).unwrap_or_default(),
            }
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The hairline that marks a run start: `──── sess-481 ──── enforce ·
/// engram ──`, filled to the terminal width.
fn hairline(paint: &Paint, width: usize, id: &str, segs: &(String, String)) -> String {
    let left = format!("──── {id} ");
    let right = if segs.0.is_empty() || segs.1.is_empty() {
        "──".to_string()
    } else {
        format!(" {} · {} ──", segs.0, segs.1)
    };
    let total = left.chars().count() + right.chars().count();
    if total >= width {
        paint.apply(Ink::Dim, &left)
    } else {
        paint.apply(
            Ink::Dim,
            &format!("{left}{}{right}", "─".repeat(width - total)),
        )
    }
}

// ───────────────────────────────────────────────────────── approval card ──

/// The card body: warn rule, the escalated chain, and the full approval
/// copy — why, blast, rollback, who. The meter line is drawn separately so
/// it can redraw in place.
fn approval_card_body(req: &ApprovalRequest, paint: &Paint) -> Vec<String> {
    let rule = paint.apply(Ink::Warn, "▐");
    let mut lines = Vec::new();
    lines.push(format!(
        "{rule} {}",
        paint.apply(
            Ink::Bold,
            &format!(
                "approval required — {} {}",
                req.tool_name,
                display_arg(&req.arguments)
            )
        )
    ));
    let policy_escalated = req.reasons.iter().any(|r| r.contains("policy"));
    let states = [
        LinkState::Pass,
        LinkState::Pass,
        if policy_escalated {
            LinkState::Escalate
        } else {
            LinkState::Pass
        },
        LinkState::Pending,
    ];
    let note = if policy_escalated {
        "policy escalates → human decides"
    } else {
        "its tier asks → human decides"
    };
    lines.push(format!(
        "{rule}   {}  {}",
        render_chain(states, true, paint),
        paint.apply(Ink::Dim, note)
    ));
    let why = if req.reasons.is_empty() {
        "—".to_string()
    } else {
        clip(&req.reasons.join("; "), 60)
    };
    lines.push(format!("{rule}   why:      {why}"));
    let blast = req
        .blast_radius
        .clone()
        .map(|b| b.note())
        .unwrap_or("not classified");
    lines.push(format!("{rule}   blast:    {blast}"));
    let rollback = match &req.rollback {
        Some(r) => {
            let mut s = clip(&r.undo, 60);
            if !r.markers.is_empty() {
                s.push_str(&format!(" (backup: {})", clip(&r.markers.join(", "), 30)));
            }
            s
        }
        None => "none declared".to_string(),
    };
    lines.push(format!("{rule}   rollback: {rollback}"));
    if let Some(who) = &req.session_label {
        lines.push(format!("{rule}   who:      {who}"));
    }
    lines.push(rule);
    lines
}

/// The 20-cell draining meter: filled in warn, empty in dim.
fn meter_bar(elapsed: u64, total: u64, cells: usize) -> (String, String) {
    let filled = ((elapsed as usize * cells) / total.max(1) as usize).min(cells);
    ("▓".repeat(filled), "░".repeat(cells - filled))
}

/// The meter face for `remaining` seconds — the line that redraws in place
/// once a second until a key or the deadline lands.
fn approval_meter(remaining: u64, paint: &Paint) -> String {
    let (filled, empty) = meter_bar(APPROVAL_SECS.saturating_sub(remaining), APPROVAL_SECS, 20);
    format!(
        "{}   {}  {}s {}{} {}",
        paint.apply(Ink::Warn, "▐"),
        paint.apply(Ink::Fg, "approve? [y/N]"),
        remaining,
        paint.apply(Ink::Warn, &filled),
        paint.apply(Ink::Dim, &empty),
        paint.apply(Ink::Dim, "── deny on timeout")
    )
}

// ───────────────────────────────────────────────────────────── picker ──

/// The per-session cost estimate in the picker: the conversation's
/// chars/4 at the current rate — an estimate, honestly labeled `~`.
fn picker_cost(
    prompt: &str,
    conversation: &[amparo_inference::ChatMessage],
    rate: Option<f64>,
) -> Option<String> {
    let rate = rate?;
    let mut tokens = estimate_tokens(prompt);
    for m in conversation {
        tokens += estimate_tokens(&m.content);
    }
    Some(format!("~${:.2}", tokens as f64 / 1_000_000.0 * rate))
}

/// One picker row: marker, number, id, prompt, age, cost.
fn picker_row(
    paint: &Paint,
    rate: Option<f64>,
    i: usize,
    task_id: &str,
    prompt: &str,
    started_at: u64,
    selected: bool,
) -> String {
    let marker = if selected {
        paint.apply(Ink::Accent, "›")
    } else {
        " ".to_string()
    };
    let ink = if selected { Ink::Fg } else { Ink::Dim };
    let mut s = format!("{marker} {}  ", i + 1);
    s.push_str(&paint.apply(ink, &format!("{:<22}", clip(task_id, 22))));
    s.push_str("  ");
    s.push_str(&paint.apply(ink, &format!("{:<30}", clip(prompt, 30))));
    s.push_str(&paint.apply(ink, &format!("  {:>7}", ago(started_at))));
    if let Some(cost) = picker_cost(prompt, conversation_empty(), rate) {
        s.push_str(&paint.apply(ink, &format!("  {cost:>8}")));
    }
    s
}

/// The picker screen: a dim header plus one row per session. Kept as a
/// plain slice of rows so the test suite can exercise the formatting
/// without constructing a checkpoint store.
fn picker_screen(
    paint: &Paint,
    rate: Option<f64>,
    rows: &[(String, String, u64)],
    sel: usize,
) -> Vec<String> {
    let mut out = vec![paint.apply(Ink::Dim, "[session] resume which session?")];
    for (i, (id, prompt, started)) in rows.iter().enumerate() {
        out.push(picker_row(paint, rate, i, id, prompt, *started, i == sel));
    }
    out
}

/// Placeholder conversation for the row's cost column — the caller passes
/// the real one through `picker_cost`.
fn conversation_empty() -> &'static [amparo_inference::ChatMessage] {
    &[]
}

/// `N s/m/h/d ago` for a unix timestamp.
fn ago(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(secs);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

// ───────────────────────────────────────────────────────── ruled reports ──

/// An OSC 8 link on a controllable terminal; plain `text (url)` otherwise.
/// Only the companion-console domains ever link.
fn link(paint: &Paint, controls: bool, url: &str, text: &str) -> String {
    if controls {
        format!(
            "\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\",
            paint.apply(Ink::Accent, text)
        )
    } else {
        format!("{text} ({url})")
    }
}

/// The policy console URL — overridable for self-hosted consoles; the
/// default lives with the org-policy client so every surface names one.
fn console_policy_url() -> String {
    std::env::var("AMPARO_CONSOLE_POLICY_URL")
        .unwrap_or_else(|_| amparo_tools::org_policy::DEFAULT_CONSOLE_POLICY_URL.to_string())
}

/// The host of a URL, hand-parsed (the CLI carries no URL crate): scheme
/// and path dropped, port kept — a direct engine wire on another port
/// must not look console-routed.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    Some(rest.split(['/', '?', '#']).next()?.to_string())
}

/// The wire host out of a banner line like
/// `wire http://127.0.0.1:47800 (profile)` — `None` when the banner is
/// not a wire line or the URL does not parse.
fn banner_policy_host(banner: &str) -> Option<String> {
    let url = banner.strip_prefix("wire ")?.split_whitespace().next()?;
    host_of(url)
}

/// The memory console URL — overridable for self-hosted consoles.
fn console_memory_url() -> String {
    std::env::var("AMPARO_CONSOLE_MEMORY_URL")
        .unwrap_or_else(|_| "https://engram.elai-intelligence.com".to_string())
}

/// Verdict display shorthand for the `/policy` report.
fn short_verdict(d: &str) -> &str {
    match d {
        "allowed" => "allow",
        "trust_blocked" => "trust block",
        "policy_denied" => "deny",
        "approval_denied" => "denied",
        "unknown_tool" => "unknown",
        other => other,
    }
}

/// The `/policy` report: engine, source, the last six verdicts, and — for
/// the built-in engine — the note that a wire engine is one flag away.
fn policy_block(
    paint: &Paint,
    controls: bool,
    live: Option<&Live>,
    recent: &[(String, String, u64)],
) -> Vec<String> {
    let rule = format!("── policy {}", "─".repeat(50));
    let mut out = vec![rule.clone()];
    let mut note: Option<Vec<String>> = None;
    match live {
        Some(l) if l.banner.policy.starts_with("wire ") => {
            let mode = if l.policy.audit_mode() {
                paint.apply(Ink::Warn, "audit — verdicts advisory")
            } else {
                "enforce".to_string()
            };
            out.push(format!("  engine:   {} · {}", l.banner.policy, mode));
            let url = console_policy_url();
            out.push(format!("  source:   {}", link(paint, controls, &url, &url)));
            // Org rules apply only when checks route through the console
            // proxy — a direct engine wire runs beside them (F4).
            if let Some(host) = banner_policy_host(&l.banner.policy) {
                let console = console_policy_url();
                if host_of(&console).as_deref() != Some(host.as_str()) {
                    out.push(format!(
                        "  warn:     checks route to {host} — org rules apply only through the console proxy"
                    ));
                }
            }
            out.push(
                "  note:     /policy deny|toggle writes org rules — the Guardrail Console applies them"
                    .to_string(),
            );
        }
        _ => {
            out.push("  engine:   built-in · deny-all default".to_string());
            out.push("  source:   built-in — no engine configured".to_string());
            let url = console_policy_url();
            note = Some(vec![
                "  note:     connect a Guardrail engine — Guardrail Console-managed policies"
                    .to_string(),
                format!("            {}", link(paint, controls, &url, &url)),
            ]);
        }
    }
    if recent.is_empty() {
        out.push("  recent:   nothing judged yet".to_string());
    } else {
        for (decision, tool, when) in recent {
            let ink = match decision.as_str() {
                "allowed" => Ink::Ok,
                "policy_denied" | "approval_denied" | "trust_blocked" => Ink::Bad,
                _ => Ink::Warn,
            };
            out.push(format!(
                "            {} {}  {}",
                paint.apply(ink, &format!("§ {}", short_verdict(decision))),
                paint.apply(Ink::Dim, &clip(tool, 26)),
                paint.apply(Ink::Dim, &ago(*when))
            ));
        }
    }
    if let Some(lines) = note {
        out.extend(lines);
    }
    out.push(rule);
    out
}

/// The `/memory` report: backend, source, and the connection note.
fn memory_block(paint: &Paint, controls: bool, live: Option<&Live>) -> Vec<String> {
    let rule = format!("── memory {}", "─".repeat(49));
    let mut out = vec![rule.clone()];
    let url = console_memory_url();
    match live {
        Some(l) if l.memory.name() == "engram" => {
            out.push("  backend:   Engram Vault".to_string());
            out.push(format!("  source:   {}", link(paint, controls, &url, &url)));
            out.push(
                "  note:     daemon reachable — searchable memory across sessions".to_string(),
            );
            out.push(
                "  note:     /memory add stores here verbatim — /memory search queries it"
                    .to_string(),
            );
        }
        _ => {
            out.push("  backend:   built-in store".to_string());
            let local = std::env::var("AMPARO_ENGRAM_URL")
                .unwrap_or_else(|_| "127.0.0.1:8787 (default)".to_string());
            out.push(format!("  source:   {local} — not answering"));
            out.push(
                "  note:     connect Engram Vault for searchable memory across sessions"
                    .to_string(),
            );
            out.push(format!("            {}", link(paint, controls, &url, &url)));
            out.push(
                "  note:     /memory add still works — the built-in store keeps this session only"
                    .to_string(),
            );
        }
    }
    out.push(rule);
    out
}

/// The `/help` block.
fn help_lines(paint: &Paint) -> Vec<String> {
    let d = |s: &str| paint.apply(Ink::Dim, s);
    vec![
        d("  type a task, press enter        run it through the gate chain"),
        d("  ↑↓                              step through the command history"),
        d("  ctrl+c                          cancel a running task (again: exit)"),
        d("  /memory add <text> | search <q> write and query memory, verbatim"),
        d("  /policy list | deny <tool>      the Guardrail Console org rules"),
        d("  /policy toggle | enforce | audit  one rule, or the org's mode"),
        d("  ! <command>                     run a shell command (outside the chain)"),
        d("  /chain   /key                   the chain, re-explained"),
        d("  /resume                         pick up a checkpointed task"),
        d("  /quit                           leave"),
    ]
}

// ──────────────────────────────────────────────────────────── the Ui ──

/// The surface mode gates what `line()` does: buffered until the banner
/// prints, normal once it streams, quiet under an approval card or picker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Buffered,
    Normal,
    Approval,
    Picker,
    Closed,
}

/// Edits the reader applies to the input line.
enum EditOp {
    Char(char),
    Backspace,
    Set(String),
    Append(String),
    Clear,
}

/// Mutable UI state, guarded by `Ui`'s single mutex: one lock, one
/// check-then-print, no interleaving.
struct UiState {
    mode: Mode,
    width: usize,
    pending: Vec<String>,
    prompt_live: bool,
    input: String,
    input_typing: bool,
    status: Option<String>,
    thinking: Option<Instant>,
    thinking_shown: bool,
    focus: bool,
    focus_lost_at: Option<Instant>,
    unfocused_lines: usize,
    banner_shown: bool,
    chain_audit_shown: bool,
    session_tokens: usize,
    live_tokens: usize,
    rate: Option<f64>,
    recent: Vec<(String, String, u64)>,
    seg_policy: String,
    seg_memory: String,
}

struct UiInner {
    out: std::io::Stdout,
    state: UiState,
}

/// The whole terminal surface. Every method locks once and writes
/// atomically, so the reader thread, the agent sinks and the run loop
/// never interleave.
struct Ui {
    paint: Paint,
    controls: bool,
    inner: Mutex<UiInner>,
}

/// The process-wide surface: the `Surface` fn-pointers route here, so
/// boot-time schedule fires reach the TUI even before the run loop starts.
static UI: OnceLock<Arc<Ui>> = OnceLock::new();

impl Ui {
    fn new(paint: Paint, controls: bool, width: usize) -> Self {
        Self {
            paint,
            controls,
            inner: Mutex::new(UiInner {
                out: std::io::stdout(),
                state: UiState {
                    mode: Mode::Buffered,
                    width,
                    pending: Vec::new(),
                    prompt_live: false,
                    input: String::new(),
                    input_typing: false,
                    status: None,
                    thinking: None,
                    thinking_shown: false,
                    focus: true,
                    focus_lost_at: None,
                    unfocused_lines: 0,
                    banner_shown: false,
                    chain_audit_shown: false,
                    session_tokens: 0,
                    live_tokens: 0,
                    rate: None,
                    recent: Vec::new(),
                    seg_policy: String::new(),
                    seg_memory: String::new(),
                },
            }),
        }
    }

    fn paint(&self) -> Paint {
        self.paint
    }

    fn controls(&self) -> bool {
        self.controls
    }

    /// The terminal width, re-measured on a controllable terminal.
    fn width(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        if self.controls {
            if let Some(w) = term_width() {
                inner.state.width = w;
            }
        }
        inner.state.width
    }

    fn mode(&self) -> Mode {
        self.inner.lock().unwrap().state.mode
    }

    fn set_mode(&self, mode: Mode) {
        self.inner.lock().unwrap().state.mode = mode;
    }

    /// One full line. In `Normal` it prints (clearing the thinking counter
    /// first); under a banner wait, an approval card or the picker it
    /// buffers, so nothing scribbles over a card or the cursor math.
    fn line(&self, s: &str) {
        let mut inner = self.inner.lock().unwrap();
        let UiInner { out, state: st } = &mut *inner;
        match st.mode {
            Mode::Normal => {
                if st.thinking_shown {
                    let _ = out.write_all(b"\r\x1b[K");
                    st.thinking_shown = false;
                }
                if !st.focus {
                    st.unfocused_lines += 1;
                }
                let _ = out.write_all(s.as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            Mode::Buffered | Mode::Approval | Mode::Picker => st.pending.push(s.to_string()),
            Mode::Closed => {}
        }
    }

    /// A block of lines as one coherent output (`/policy`, `/help`).
    fn block(&self, lines: &[String]) {
        for l in lines {
            self.line(l);
        }
    }

    /// Prints the boot banner once and flips `Buffered` to `Normal`,
    /// flushing whatever the boot wire queued before the banner was ready.
    fn show_banner(&self, lines: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.banner_shown {
            return;
        }
        inner.state.banner_shown = true;
        for l in lines {
            let _ = inner.out.write_all(l.as_bytes());
            let _ = inner.out.write_all(b"\n");
        }
        if inner.state.mode == Mode::Buffered {
            inner.state.mode = Mode::Normal;
            let pending = std::mem::take(&mut inner.state.pending);
            for p in pending {
                let _ = inner.out.write_all(p.as_bytes());
                let _ = inner.out.write_all(b"\n");
            }
        }
        let _ = inner.out.flush();
    }

    // ── prompt ──

    /// Marks the prompt live and redraws it (idle title, block cursor).
    fn prompt_ready(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.prompt_live = true;
        inner.state.thinking = None;
        if inner.state.mode == Mode::Normal {
            render_prompt(&mut inner, &self.paint, self.controls);
        }
        drop(inner);
        self.set_title("amparo · idle");
    }

    /// A task starts: the prompt goes quiet, the streaming cursor takes
    /// over, the live token counter resets.
    fn task_begin(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.prompt_live = false;
        inner.state.thinking = Some(Instant::now());
        inner.state.thinking_shown = false;
        inner.state.live_tokens = 0;
        if self.controls {
            let _ = inner.out.write_all(b"\x1b[6 q");
            let _ = inner.out.flush();
        }
    }

    /// A task's report lands: the session total takes the report's count
    /// and the live estimate resets (they measured the same tokens).
    fn task_end_tokens(&self, tokens: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.session_tokens += tokens;
        inner.state.live_tokens = 0;
    }

    fn add_live_tokens(&self, n: usize) {
        self.inner.lock().unwrap().state.live_tokens += n;
    }

    fn set_rate(&self, rate: Option<f64>) {
        self.inner.lock().unwrap().state.rate = rate;
    }

    fn rate(&self) -> Option<f64> {
        self.inner.lock().unwrap().state.rate
    }

    fn cost(&self) -> Option<f64> {
        let st = &self.inner.lock().unwrap().state;
        st.rate
            .map(|r| (st.session_tokens + st.live_tokens) as f64 / 1_000_000.0 * r)
    }

    fn set_status(&self, status: Option<String>) {
        self.inner.lock().unwrap().state.status = status;
    }

    fn prompt_live(&self) -> bool {
        self.inner.lock().unwrap().state.prompt_live
    }

    fn task_active(&self) -> bool {
        self.inner.lock().unwrap().state.thinking.is_some()
    }

    fn input(&self) -> String {
        self.inner.lock().unwrap().state.input.clone()
    }

    /// Applies an edit and redraws the prompt when it is live.
    fn edit(&self, op: EditOp) {
        let mut inner = self.inner.lock().unwrap();
        match op {
            EditOp::Char(c) => inner.state.input.push(c),
            EditOp::Backspace => {
                inner.state.input.pop();
            }
            EditOp::Set(s) => inner.state.input = s,
            EditOp::Append(s) => inner.state.input.push_str(&s),
            EditOp::Clear => inner.state.input.clear(),
        }
        inner.state.input_typing = !inner.state.input.is_empty();
        if inner.state.prompt_live && inner.state.mode == Mode::Normal {
            render_prompt(&mut inner, &self.paint, self.controls);
        }
    }

    /// Commits the input line: takes it, closes the prompt, moves to a
    /// fresh line.
    fn submit_line(&self) -> String {
        let mut inner = self.inner.lock().unwrap();
        let line = std::mem::take(&mut inner.state.input);
        inner.state.input_typing = false;
        inner.state.prompt_live = false;
        let _ = inner.out.write_all(b"\n");
        let _ = inner.out.flush();
        line
    }

    /// The thinking tick: after one quiet second, a dim elapsed counter
    /// sits where the next `[turn]` line will land.
    fn think_tick(&self) {
        if !self.controls {
            return;
        }
        let paint = self.paint;
        let mut inner = self.inner.lock().unwrap();
        let UiInner { out, state: st } = &mut *inner;
        let Some(since) = st.thinking else { return };
        if st.mode != Mode::Normal || since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let secs = since.elapsed().as_secs();
        let _ = out.write_all(b"\r\x1b[K");
        let _ = out.write_all(paint.apply(Ink::Dim, &format!("{secs}s")).as_bytes());
        st.thinking_shown = true;
        let _ = out.flush();
    }

    // ── approval ──

    /// Prints the card body under `Approval` mode (set first, so
    /// everything else buffers).
    fn card(&self, lines: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.mode != Mode::Approval {
            return;
        }
        for l in lines {
            let _ = inner.out.write_all(l.as_bytes());
            let _ = inner.out.write_all(b"\n");
        }
        let _ = inner.out.flush();
    }

    /// Redraws the meter in place (no newline — the cursor stays on it).
    fn meter(&self, line: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.mode != Mode::Approval {
            return;
        }
        let _ = inner.out.write_all(b"\r\x1b[K");
        let _ = inner.out.write_all(line.as_bytes());
        let _ = inner.out.flush();
    }

    /// Resolves an approval: clears the meter, speaks the outcome
    /// first-person, restores the streaming cursor, reopens the stream.
    fn resolve_approval(&self, line: &str) {
        let mut inner = self.inner.lock().unwrap();
        let UiInner { out, state: st } = &mut *inner;
        if self.controls {
            let _ = out.write_all(b"\r\x1b[K");
        }
        let _ = out.write_all(line.as_bytes());
        let _ = out.write_all(b"\n");
        st.mode = Mode::Normal;
        if self.controls {
            let _ = out.write_all(b"\x1b[6 q\x1b]12;#e2e8f0\x07");
        }
        let pending = std::mem::take(&mut st.pending);
        for p in pending {
            let _ = out.write_all(p.as_bytes());
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
    }

    /// The amber underline cursor + matching title-bar tint while an
    /// approval waits.
    fn cursor_approval(&self) {
        if !self.controls {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.out.write_all(b"\x1b[3 q\x1b]12;#f59e0b\x07");
        let _ = inner.out.flush();
    }

    fn bell(&self) {
        if !self.controls {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.out.write_all(b"\x07");
        let _ = inner.out.flush();
    }

    fn set_title(&self, t: &str) {
        if !self.controls {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let _ = write!(inner.out, "\x1b]0;{t}\x07");
        let _ = inner.out.flush();
    }

    // ── picker ──

    fn picker_begin(&self, lines: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.mode != Mode::Picker {
            return;
        }
        for l in lines {
            let _ = inner.out.write_all(l.as_bytes());
            let _ = inner.out.write_all(b"\n");
        }
        let _ = inner.out.flush();
    }

    /// Repaints the picker block with cursor-up lines — no scroll region.
    fn picker_redraw(&self, lines: &[String], height: usize) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.mode != Mode::Picker {
            return;
        }
        if self.controls {
            let _ = write!(inner.out, "\x1b[{height}A");
        }
        for l in lines {
            let _ = inner.out.write_all(b"\r\x1b[K");
            let _ = inner.out.write_all(l.as_bytes());
            let _ = inner.out.write_all(b"\n");
        }
        let _ = inner.out.flush();
    }

    fn picker_end(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.mode == Mode::Picker {
            inner.state.mode = Mode::Normal;
            let pending = std::mem::take(&mut inner.state.pending);
            for p in pending {
                let _ = inner.out.write_all(p.as_bytes());
                let _ = inner.out.write_all(b"\n");
            }
            let _ = inner.out.flush();
        }
    }

    // ── focus ──

    fn focus_lost(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.focus = false;
        inner.state.focus_lost_at = Some(Instant::now());
    }

    /// On refocus after more than 15 minutes away, a first-person
    /// briefing: what arrived while the terminal was dark.
    fn focus_gained(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let st = &mut inner.state;
        st.focus = true;
        let away = st
            .focus_lost_at
            .take()
            .map(|t| t.elapsed())
            .filter(|d| *d > Duration::from_secs(15 * 60));
        if let Some(d) = away {
            let m = d.as_secs() / 60;
            let n = st.unfocused_lines;
            st.unfocused_lines = 0;
            Some(if n == 0 {
                format!("[away] you were gone {m}m — nothing happened")
            } else {
                format!("[away] you were gone {m}m — {n} line(s) arrived while you were away")
            })
        } else {
            None
        }
    }

    // ── state readers ──

    fn record_verdict(&self, decision: &str, tool: &str) {
        let st = &mut self.inner.lock().unwrap().state;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        st.recent
            .push((decision.to_string(), tool.to_string(), now));
        if st.recent.len() > 6 {
            st.recent.remove(0);
        }
    }

    fn recent(&self) -> Vec<(String, String, u64)> {
        self.inner.lock().unwrap().state.recent.clone()
    }

    fn set_segments(&self, policy: String, memory: String) {
        let st = &mut self.inner.lock().unwrap().state;
        st.seg_policy = policy;
        st.seg_memory = memory;
    }

    fn segments(&self) -> (String, String) {
        let st = &self.inner.lock().unwrap().state;
        (st.seg_policy.clone(), st.seg_memory.clone())
    }

    /// Compare-and-set for the one-time mid-stream `[chain]` reprint:
    /// returns whether the audit state was already announced.
    fn chain_audit_reprint(&self) -> bool {
        let st = &mut self.inner.lock().unwrap().state;
        if st.chain_audit_shown {
            true
        } else {
            st.chain_audit_shown = true;
            false
        }
    }

    // ── lifecycle ──

    fn enable_modes(&self) {
        if !self.controls {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.out.write_all(b"\x1b[?1004h\x1b[?2004h\x1b[22;2t");
        let _ = inner.out.flush();
    }

    fn shutdown(&self) {
        if !self.controls {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.state.mode = Mode::Closed;
        // Leave the terminal clean: finish the line, pop the title,
        // release the modes, restore the block cursor and colors.
        let _ = inner
            .out
            .write_all(b"\r\x1b[K\n\x1b[23;2t\x1b[?1004l\x1b[?2004l\x1b[2 q\x1b]12;\x07\x1b[0m");
        let _ = inner.out.flush();
    }
}

/// Draws the prompt: `› ` accent, the input (or the `Amparo█` block glyph
/// at rest), the dim status right-aligned behind a cursor save/restore,
/// wrapped in DEC 2026 so a tight width never wraps mid-redraw.
fn render_prompt(inner: &mut UiInner, paint: &Paint, controls: bool) {
    if !controls {
        return;
    }
    let st = &mut inner.state;
    let out = &mut inner.out;
    let _ = out.write_all(b"\r\x1b[K\x1b[?2026h");
    let mut left = paint.apply(Ink::Accent, "› ");
    let left_len = if st.input.is_empty() && !st.input_typing {
        left.push_str(&paint.apply(Ink::Fg, "Amparo"));
        left.push_str(&paint.apply(Ink::Accent, "█"));
        2 + 7
    } else {
        left.push_str(&paint.apply(Ink::Fg, &st.input));
        2 + st.input.chars().count()
    };
    let _ = out.write_all(left.as_bytes());
    if let Some(status) = &st.status {
        let s_len = plain_len(status);
        if left_len + s_len + 1 < st.width {
            let pad = st.width - left_len - s_len - 1;
            let _ = out.write_all(" ".repeat(pad).as_bytes());
            let _ = out.write_all(b"\x1b7"); // save the cursor
            let _ = out.write_all(status.as_bytes());
            let _ = out.write_all(b"\x1b8"); // restore it at the input
        }
    }
    let _ = out.write_all(b"\x1b[?2026l");
    let cursor = if st.input_typing {
        b"\x1b[6 q"
    } else {
        b"\x1b[2 q"
    };
    let _ = out.write_all(cursor);
    let _ = out.flush();
}

/// The `Surface` banner hook: paints the banner through the shared UI (or
/// falls back to `run`'s stderr banner before the UI exists).
fn tui_banner(info: &BannerInfo) {
    match UI.get() {
        Some(ui) => {
            let lines = banner_lines(info, &ui.paint());
            ui.show_banner(&lines);
        }
        None => run::boot_banner(info),
    }
}

/// The `Surface` line/notice hooks.
fn tui_line(line: &str) {
    match UI.get() {
        Some(ui) => ui.line(line),
        None => eprintln!("{line}"),
    }
}

fn tui_notice(line: &str) {
    tui_line(line)
}

// ─────────────────────────────────────────────────────────────── sink ──

/// One gutter row per tool call, keyed by call id: the dim `◆▲§◉` row at
/// request time completes green at execution, or colors at the link that
/// stopped it.
struct GutterRow {
    desc: String,
    escalated: bool,
}

/// The TUI's event sink: gutter rows for the gate chain, hairlines for run
/// starts, `[tag]` lines for everything else.
struct TuiSink {
    ui: Arc<Ui>,
    task_id: String,
    rows: Mutex<HashMap<String, GutterRow>>,
}

impl TuiSink {
    fn new(ui: Arc<Ui>, task_id: String) -> Self {
        Self {
            ui,
            task_id,
            rows: Mutex::new(HashMap::new()),
        }
    }
}

impl EventSink for TuiSink {
    fn emit(&self, event: &AgentEvent) {
        let paint = self.ui.paint();
        match event {
            AgentEvent::ToolCallRequested { call } => {
                let arg = display_arg(&call.arguments);
                let desc = clip(&format!("{} {}", call.name, arg), 60);
                self.rows.lock().unwrap().insert(
                    call.id.clone(),
                    GutterRow {
                        desc: desc.clone(),
                        escalated: false,
                    },
                );
                let chain = render_chain([LinkState::Unreached; 4], false, &paint);
                self.ui
                    .line(&format!("{chain}  {}", paint.apply(Ink::Dim, &desc)));
                self.ui.add_live_tokens(estimate_tokens(
                    &serde_json::to_string(&call.arguments).unwrap_or_default(),
                ));
                self.ui.set_title(&format!(
                    "amparo · {} · running: {}",
                    self.task_id, call.name
                ));
            }
            AgentEvent::ToolGate {
                call_id,
                tool_name,
                decision,
                reasons,
            } => {
                let (desc, escalated) = self
                    .rows
                    .lock()
                    .unwrap()
                    .remove(call_id)
                    .map(|r| (r.desc, r.escalated))
                    .unwrap_or_default();
                match gate_states(decision, escalated) {
                    Some(states) => {
                        let suffix = gate_suffix(decision, reasons);
                        let chain = render_chain(states, false, &paint);
                        self.ui.line(&format!(
                            "{chain}  {}  {}",
                            paint.apply(Ink::Dim, &desc),
                            paint.apply(Ink::Bad, &suffix)
                        ));
                        self.ui
                            .record_verdict(decision, &format!("{} {}", tool_name, desc));
                    }
                    // An allowed call needs no row of its own — it
                    // completes green when it executes.
                    None if decision == "allowed" => {}
                    // A decision this surface does not know yet: let the
                    // plain event speak, and remember it for /policy.
                    None => {
                        self.ui.record_verdict(decision, tool_name);
                        self.ui.line(&format_event(event));
                    }
                }
            }
            AgentEvent::ApprovalRequested { call_id, .. } => {
                if let Some(row) = self.rows.lock().unwrap().get_mut(call_id) {
                    row.escalated = true;
                }
                let desc = self
                    .rows
                    .lock()
                    .unwrap()
                    .get(call_id)
                    .map(|r| r.desc.clone())
                    .unwrap_or_default();
                let chain = render_chain(
                    [
                        LinkState::Pass,
                        LinkState::Pass,
                        LinkState::Escalate,
                        LinkState::Pending,
                    ],
                    false,
                    &paint,
                );
                self.ui.line(&format!(
                    "{chain}  {}  {}",
                    paint.apply(Ink::Dim, &desc),
                    paint.apply(Ink::Warn, "escalated")
                ));
            }
            // The gate itself speaks the resolution line.
            AgentEvent::ApprovalResolved { .. } => {}
            AgentEvent::ToolExecuted { result } => {
                let desc = self
                    .rows
                    .lock()
                    .unwrap()
                    .remove(&result.tool_call_id)
                    .map(|r| r.desc);
                match desc {
                    Some(desc) => {
                        let chain = render_chain([LinkState::Pass; 4], false, &paint);
                        self.ui.line(&format!(
                            "{chain}  {}  {}",
                            paint.apply(Ink::Dim, &desc),
                            paint.apply(Ink::Dim, &format!("{}ms", result.duration_ms))
                        ));
                    }
                    None => self.ui.line(&format_event(event)),
                }
            }
            AgentEvent::TaskStarted { task_id, .. } => {
                let id = task_id.as_deref().unwrap_or(&self.task_id).to_string();
                self.ui
                    .line(&hairline(&paint, self.ui.width(), &id, &self.ui.segments()));
                self.ui.line(&format_event(event));
            }
            AgentEvent::AssistantTurn { content, .. } => {
                self.ui.add_live_tokens(estimate_tokens(content));
                self.ui.line(&format_event(event));
            }
            AgentEvent::FinalAnswer { content } => {
                self.ui.add_live_tokens(estimate_tokens(content));
                self.ui.line(&format_event(event));
            }
            other => self.ui.line(&format_event(other)),
        }
    }
}

// ──────────────────────────────────────────────────────── approval gate ──

/// The TUI's human gate: renders the approval card and waits for a single
/// keypress under a 60-second fail-closed deadline. Piped (or a terminal
/// we cannot control), it prints the card and denies — there is no human
/// to ask.
struct TuiApprovalGate {
    ui: Arc<Ui>,
    keys: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    /// One card on screen at a time: a second approval waits for the first
    /// to resolve, so two cards never interleave.
    slot: Arc<tokio::sync::Mutex<()>>,
}

impl TuiApprovalGate {
    fn new(
        ui: Arc<Ui>,
        keys: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    ) -> Self {
        Self {
            ui,
            keys,
            slot: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait]
impl ApprovalGate for TuiApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        let _slot = self.slot.lock().await;
        let paint = self.ui.paint();
        self.ui.set_mode(Mode::Approval);
        self.ui.set_title("amparo · awaiting approval");
        self.ui.cursor_approval();
        self.ui.bell();
        self.ui.card(&approval_card_body(request, &paint));
        if !self.ui.controls() {
            // Piped or a degraded terminal: the card prints, then the
            // request fails closed — no terminal to ask.
            self.ui.card(&[approval_meter(APPROVAL_SECS, &paint)]);
            let denied = format!(
                "[approval] {}",
                paint.apply(Ink::Bad, "no terminal to ask — denied (fail-closed)")
            );
            self.ui.resolve_approval(&denied);
            return false;
        }
        self.ui.meter(&approval_meter(APPROVAL_SECS, &paint));
        let mut keys = self.keys.lock().await;
        let approved = match keys.as_mut() {
            None => {
                let denied = format!(
                    "[approval] {}",
                    paint.apply(Ink::Bad, "no key input — denied (fail-closed)")
                );
                self.ui.resolve_approval(&denied);
                return false;
            }
            Some(rx) => {
                while rx.try_recv().is_ok() {} // stale keys never survive
                let deadline = tokio::time::Instant::now() + Duration::from_secs(APPROVAL_SECS);
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // the first tick fires immediately
                loop {
                    tokio::select! {
                        key = rx.recv() => match key {
                            Some('y') | Some('Y') => break true,
                            Some(_) | None => break false,
                        },
                        _ = interval.tick() => {
                            let remaining = deadline
                                .saturating_duration_since(tokio::time::Instant::now())
                                .as_secs();
                            self.ui.meter(&approval_meter(remaining, &paint));
                        },
                        _ = tokio::time::sleep_until(deadline) => break false,
                    }
                }
            }
        };
        let line = if approved {
            format!(
                "[approval] {}",
                paint.apply(Ink::Ok, "you granted this — proceeding")
            )
        } else {
            format!(
                "[approval] {}",
                paint.apply(Ink::Bad, "you denied this — I won't touch it")
            )
        };
        self.ui.resolve_approval(&line);
        drop(keys);
        approved
    }
}

// ───────────────────────────────────────────────────────────── the live ──

/// The wired facts behind the status line and the `/` reports, refreshed
/// after every wire.
struct Live {
    banner: BannerInfo,
    policy: Arc<dyn PolicyEngine>,
    memory: Arc<dyn Memory>,
}

/// The short register segments: policy mode and memory backend.
fn live_segments(live: &Live) -> (String, String) {
    let policy = if live.policy.audit_mode() {
        "audit".to_string()
    } else if live.banner.policy.starts_with("wire ") {
        "enforce".to_string()
    } else if live.banner.policy.starts_with("deny-all") {
        "deny-all".to_string()
    } else {
        "allow-all".to_string()
    };
    let memory = if live.memory.name() == "engram" {
        "engram".to_string()
    } else {
        "built-in".to_string()
    };
    (policy, memory)
}

/// Refreshes the live facts and the UI's derived state after a wire.
fn update_live(ui: &Ui, live: &Mutex<Option<Live>>, wired: &WiredRun) {
    let l = Live {
        banner: wired.banner.clone(),
        policy: Arc::clone(&wired.policy),
        memory: Arc::clone(&wired.memory),
    };
    let segs = live_segments(&l);
    ui.set_segments(segs.0, segs.1);
    ui.set_rate(wired.cost_rate);
    *live.lock().unwrap() = Some(l);
}

/// Re-prints the `[chain]` line once, the first time the wire engine
/// answers in audit mode — the surface's live deviation signal.
fn maybe_reprint_chain(ui: &Ui, live: &Option<Live>) {
    let audit = live
        .as_ref()
        .map(|l| l.policy.audit_mode())
        .unwrap_or(false);
    if audit && !ui.chain_audit_reprint() {
        if let Some(l) = live {
            ui.line(&format!(
                "[chain] {}",
                chain_line(&l.banner, true, &ui.paint())
            ));
        }
    }
}

// ───────────────────────────────────────────────────────────── wiring ──

/// Builds the TUI's surface: the TUI sink, the TUI approval gate, and the
/// shared banner/line hooks.
fn tui_surface(
    ui: &Arc<Ui>,
    task_id: &str,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
) -> (Surface, Arc<TuiSink>) {
    let sink = Arc::new(TuiSink::new(Arc::clone(ui), task_id.to_string()));
    let gate = Arc::new(TuiApprovalGate::new(Arc::clone(ui), Arc::clone(keys)));
    let surface = Surface {
        events: Arc::clone(&sink) as Arc<dyn EventSink>,
        approval: Some((gate, "you, at this terminal — 60s fail-closed".to_string())),
        banner: tui_banner,
        line: tui_line,
        notice: tui_notice,
    };
    (surface, sink)
}

/// The boot wire: prints the banner through the surface and resolves the
/// chain facts before the prompt opens. Its schedule promises run beside
/// the session, so the prompt is never delayed by a firing promise.
async fn boot_wire(
    ui: &Arc<Ui>,
    flags: &RunFlags,
    live: &Arc<Mutex<Option<Live>>>,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
) -> Result<(), String> {
    let (surface, _sink) = tui_surface(ui, "boot", keys);
    let mut wired = run::wire_with(flags, run::new_task_id(), surface).await?;
    update_live(ui, live, &wired);
    let fires = wired.schedule_fires.drain(..).collect::<Vec<_>>();
    tokio::spawn(async move {
        for f in fires {
            let _ = f.await;
        }
    });
    Ok(())
}

/// Runs one task end-to-end: wire, update the live facts, run the loop
/// (or resume a checkpoint), await schedule fires, finish. Honors the
/// cancel watch at every await boundary.
async fn run_one_task(
    ui: &Arc<Ui>,
    flags: &RunFlags,
    prompt: &str,
    resume: Option<&Checkpoint>,
    live: &Arc<Mutex<Option<Live>>>,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), String> {
    let task_id = match resume {
        Some(c) => c.task_id.clone(),
        None => run::new_task_id(),
    };
    let (surface, _sink) = tui_surface(ui, &task_id, keys);
    let mut wired = tokio::select! {
        r = run::wire_with(flags, task_id.clone(), surface) => r?,
        _ = cancel.changed() => return Ok(()),
    };
    update_live(ui, live, &wired);
    ui.set_title(&format!("amparo · {task_id} · running"));
    let fires = wired.schedule_fires.drain(..).collect::<Vec<_>>();
    let report = tokio::select! {
        r = async {
            match resume {
                Some(c) => wired.agent.resume(c.clone()).await,
                None => wired.agent.run(prompt.to_string()).await,
            }
        } => Some(r),
        _ = cancel.changed() => None,
    };
    for f in fires {
        let _ = f.await;
    }
    match report {
        None => Ok(()), // canceled — the canceling path already spoke
        Some(report) => tui_finish(ui, &mut wired, report).await,
    }
}

/// The TUI twin of `run::finish`: every report line routed through the
/// surface instead of raw eprintln, and the final answer stays in the
/// stream (the `[answer]` event already printed it).
async fn tui_finish(ui: &Arc<Ui>, wired: &mut WiredRun, report: AgentReport) -> Result<(), String> {
    if let Some(nb) = &wired.notebook {
        nb.flush().await;
    }
    if let Some(line) = format_cost_line(report.tokens_estimated, wired.cost_rate) {
        let paint = ui.paint();
        ui.line(&paint.apply(Ink::ItalicDim, &format!("[report] {line}")));
    }
    if let Some(tool) = &wired.spawn_tool {
        if !tool.reports().is_empty() {
            ui.line(&format!(
                "[swarm] {}",
                tool.summary_line(report.tool_calls, report.tokens_estimated, wired.cost_rate)
            ));
        }
    }
    let result = match report.status {
        TaskStatus::Complete => {
            ui.line(&format!(
                "[report] complete — {} step(s), verification: {}",
                report.steps_used,
                report
                    .verification
                    .map(|v| v.decision)
                    .unwrap_or_else(|| "n/a".to_string())
            ));
            Ok(())
        }
        TaskStatus::Failed => {
            ui.line(&format!("[report] failed — {} step(s)", report.steps_used));
            Err(format!(
                "task failed: {}",
                report
                    .final_answer
                    .unwrap_or_else(|| "no final answer".to_string())
            ))
        }
    };
    ui.task_end_tokens(report.tokens_estimated);
    result
}

// ──────────────────────────────────────────────────────────── the loop ──

enum TaskOutcome {
    Done(Result<(), String>),
    Exit,
}

/// Runs a task while the reader can interrupt: first Ctrl-C cancels (the
/// checkpoint stays), a second within two seconds exits.
async fn run_task_with_cancel(
    ui: &Arc<Ui>,
    flags: &RunFlags,
    prompt: &str,
    resume: Option<&Checkpoint>,
    live: &Arc<Mutex<Option<Live>>>,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    main_rx: &mut mpsc::UnboundedReceiver<ReaderMsg>,
) -> TaskOutcome {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let ui2 = Arc::clone(ui);
    let live2 = Arc::clone(live);
    let keys2 = Arc::clone(keys);
    let prompt2 = prompt.to_string();
    let resume2 = resume.cloned();
    let flags2 = flags.clone();
    let mut handle = tokio::spawn(async move {
        run_one_task(
            &ui2,
            &flags2,
            &prompt2,
            resume2.as_ref(),
            &live2,
            &keys2,
            cancel_rx,
        )
        .await
    });
    loop {
        tokio::select! {
            r = &mut handle => {
                return TaskOutcome::Done(r.unwrap_or(Err("task aborted".to_string())));
            }
            msg = main_rx.recv() => match msg {
                Some(ReaderMsg::Cancel) => {
                    let _ = cancel_tx.send(true);
                    ui.line("[task] canceled — the checkpoint stays; /resume continues");
                    let window = tokio::time::Instant::now() + Duration::from_secs(2);
                    loop {
                        tokio::select! {
                            r = &mut handle => {
                                return TaskOutcome::Done(r.unwrap_or(Err("task aborted".to_string())));
                            }
                            msg = main_rx.recv() => match msg {
                                Some(ReaderMsg::Cancel) | Some(ReaderMsg::Quit)
                                    if tokio::time::Instant::now() < window => return TaskOutcome::Exit,
                                None => return TaskOutcome::Exit,
                                Some(_) => {}
                            },
                            _ = tokio::time::sleep_until(window) => {},
                        }
                    }
                }
                Some(ReaderMsg::Quit) | None => {
                    let _ = cancel_tx.send(true);
                    let _ = (&mut handle).await;
                    return TaskOutcome::Exit;
                }
                Some(ReaderMsg::Focus(true)) => {
                    if let Some(l) = ui.focus_gained() {
                        ui.line(&l);
                    }
                }
                Some(ReaderMsg::Focus(false)) => {
                    ui.focus_lost();
                }
                Some(_) => {}
            },
        }
    }
}

enum CommandOut {
    Task(String),
    Resume(Checkpoint),
    Handled,
    Quit,
}

/// `/memory add|search` — the human's direct write into the memory store.
///
/// The human keystroke is stored verbatim: it sits at the top of the gate
/// chain, above the agent's own PII-stripped `MemoryWriteTool` (I1/I3).
/// The store Arc is cloned out of the live lock before any await — the
/// std mutex never spans one.
async fn cmd_memory(ui: &Arc<Ui>, live: &Arc<Mutex<Option<Live>>>, rest: &str) {
    let (sub, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    let memory = {
        let guard = live.lock().unwrap();
        guard.as_ref().map(|l| Arc::clone(&l.memory))
    };
    let Some(memory) = memory else {
        ui.line("[memory] not wired yet — the store resolves at boot");
        return;
    };
    match sub {
        "add" => {
            let text = arg.trim();
            if text.is_empty() {
                ui.line("[memory] usage: /memory add <text> — stored verbatim");
                return;
            }
            match memory.store(text.to_string()).await {
                Ok(id) => {
                    let note = if memory.name() == "engram" {
                        String::new()
                    } else {
                        " — built-in store (session-only)".to_string()
                    };
                    ui.line(&format!("[memory] stored {id}{note}"));
                }
                Err(reason) => ui.line(&format!("[memory] filtered — {reason}")),
            }
        }
        "search" => {
            let query = arg.trim();
            if query.is_empty() {
                ui.line("[memory] usage: /memory search <query>");
                return;
            }
            let hits = memory.search(query, 5).await;
            if hits.is_empty() {
                ui.line("[memory] no hits");
                return;
            }
            ui.line(&format!("[memory] {} hit(s):", hits.len()));
            for h in &hits {
                ui.line(&format!("  {}   {}", clip(&h.content, 56), h.id));
            }
        }
        other => ui.line(&format!(
            "[memory] unknown '{other}' — /memory add <text> | /memory search <query>"
        )),
    }
}

/// The `/policy` errors, in the surface's voice.
fn policy_err_line(e: &amparo_tools::OrgPolicyError) -> String {
    match e {
        amparo_tools::OrgPolicyError::NotConnected(hint) => {
            format!("[policy] not connected — {hint}")
        }
        amparo_tools::OrgPolicyError::Http { status, message } => {
            format!("[policy] HTTP {status} — {message}")
        }
        amparo_tools::OrgPolicyError::BadResponse(m) => format!("[policy] bad response — {m}"),
    }
}

/// `/policy list|deny|toggle|enforce|audit` — the human's direct write
/// into the Guardrail Console org rules.
///
/// Harden-only by construction: the console accepts deny rules, never
/// allows, so nothing typed here can weaken the gate chain. The rules
/// apply only when the run's checks route through the console proxy — a
/// run with no wire engine (or wired directly at the engine) has nothing
/// for them to act on, and the command says so.
async fn cmd_policy(ui: &Arc<Ui>, live: &Arc<Mutex<Option<Live>>>, rest: &str) {
    let wired = {
        let guard = live.lock().unwrap();
        guard
            .as_ref()
            .map(|l| l.banner.policy.clone())
            .is_some_and(|p| p.starts_with("wire "))
    };
    if !wired {
        ui.line("[policy] not wired yet — connect a Guardrail engine first (/key)");
        return;
    }
    let client = match amparo_tools::OrgPolicyClient::from_env() {
        Ok(c) => c,
        Err(e) => {
            ui.line(&policy_err_line(&e));
            return;
        }
    };
    let (sub, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    match sub {
        "list" => {
            let org = match client.current_org().await {
                Ok(o) => o,
                Err(e) => {
                    ui.line(&policy_err_line(&e));
                    return;
                }
            };
            match client.list_rules().await {
                Ok(rules) => {
                    ui.line(&format!(
                        "[policy] {} org rule(s) · org mode: {}",
                        rules.len(),
                        org.enforce_mode
                    ));
                    for r in &rules {
                        let state = if r.enabled { "enabled" } else { "disabled" };
                        let reason = if r.reason.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", r.reason)
                        };
                        ui.line(&format!("  {}  {}{}", clip(&r.tool_name, 28), state, reason));
                    }
                }
                Err(e) => ui.line(&policy_err_line(&e)),
            }
        }
        "deny" => {
            let (tool, reason) = arg
                .split_once(' ')
                .map(|(t, r)| (t.trim(), r.trim()))
                .unwrap_or((arg.trim(), ""));
            match client.deny(tool, reason).await {
                Ok(rule) => ui.line(&format!(
                    "[policy] denied {} — rule {} active",
                    rule.tool_name, rule.id
                )),
                Err(amparo_tools::OrgPolicyError::Http { status: 409, message }) => {
                    ui.line(&format!(
                        "[policy] '{tool}' already has a rule ({message}) — /policy toggle {tool}"
                    ));
                }
                Err(e) => ui.line(&policy_err_line(&e)),
            }
        }
        "toggle" => {
            let tool = arg.trim();
            match client.toggle(tool).await {
                Ok(rule) => {
                    let state = if rule.enabled { "enabled" } else { "disabled" };
                    ui.line(&format!("[policy] {} {state}", rule.tool_name));
                }
                Err(e) => ui.line(&policy_err_line(&e)),
            }
        }
        "enforce" | "audit" => match client.set_mode(sub).await {
            Ok(()) => ui.line(&format!("[policy] org mode: {sub}")),
            Err(e) => ui.line(&policy_err_line(&e)),
        },
        other => ui.line(&format!(
            "[policy] unknown '{other}' — /policy list | deny <tool> | toggle <tool> | enforce | audit"
        )),
    }
}

/// The platform shell behind `!`: `sh -c` on unix, `cmd /C` elsewhere.
#[cfg(unix)]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

/// The platform shell behind `!`: `cmd /C` (non-unix).
#[cfg(not(unix))]
fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("cmd");
    c.arg("/C").arg(cmd);
    c
}

/// `! <cmd>` — a shell escape at the prompt.
///
/// Interactive: the raw-mode guard drops so the child owns a normal
/// terminal (its output — or its own TUI — renders), then raw mode
/// re-enters. Piped: spawn and wait; the child inherits stdio, so its
/// output lands inline in the stream. The child runs OUTSIDE the gate
/// chain — it is the human's own keystroke, like any shell.
async fn shell_escape(ui: &Arc<Ui>, raw: &RawSlot, cmd: &str) {
    if cmd.is_empty() {
        ui.line("[shell] usage: ! <command> — runs in your shell, outside the gate chain");
        return;
    }
    #[cfg(unix)]
    raw.drop_for_child();
    let status = shell_command(cmd).status().await;
    #[cfg(unix)]
    raw.reenter();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => ui.line(&format!("[shell] exited {s}")),
        Err(e) => ui.line(&format!("[shell] failed to run — {e}")),
    }
}

/// The `/` commands. `/resume` opens the picker (which borrows the reader
/// channel for its keys); the `/memory` and `/policy` subcommands write
/// into the sibling products; `!` runs a shell command outside the chain.
async fn handle_command(
    ui: &Arc<Ui>,
    live: &Arc<Mutex<Option<Live>>>,
    raw: &RawSlot,
    line: &str,
    main_rx: &mut mpsc::UnboundedReceiver<ReaderMsg>,
) -> CommandOut {
    match line {
        "/quit" | "/exit" => CommandOut::Quit,
        "/help" => {
            ui.block(&help_lines(&ui.paint()));
            CommandOut::Handled
        }
        "/key" => {
            ui.line(&key_line(&ui.paint(), true));
            CommandOut::Handled
        }
        "/chain" => {
            let guard = live.lock().unwrap();
            match guard.as_ref() {
                Some(l) => {
                    let audit = l.policy.audit_mode();
                    ui.line(&format!(
                        "[chain] {}",
                        chain_line(&l.banner, audit, &ui.paint())
                    ));
                }
                None => ui.line("[chain] — not wired yet"),
            }
            CommandOut::Handled
        }
        "/policy" => {
            let guard = live.lock().unwrap();
            let recent = ui.recent();
            ui.block(&policy_block(
                &ui.paint(),
                ui.controls(),
                guard.as_ref(),
                &recent,
            ));
            CommandOut::Handled
        }
        "/memory" => {
            let guard = live.lock().unwrap();
            ui.block(&memory_block(&ui.paint(), ui.controls(), guard.as_ref()));
            CommandOut::Handled
        }
        "/resume" => match run_picker(ui, live, main_rx).await {
            Some(cp) => CommandOut::Resume(cp),
            None => CommandOut::Handled,
        },
        other if other.starts_with("/memory ") => {
            cmd_memory(ui, live, &other["/memory ".len()..]).await;
            CommandOut::Handled
        }
        other if other.starts_with("/policy ") => {
            cmd_policy(ui, live, &other["/policy ".len()..]).await;
            CommandOut::Handled
        }
        other if other.starts_with('!') => {
            shell_escape(ui, raw, other[1..].trim()).await;
            CommandOut::Handled
        }
        other if other.starts_with('/') => {
            ui.line(&format!(
                "[note] unknown command '{other}' — /help lists them"
            ));
            CommandOut::Handled
        }
        other => CommandOut::Task(other.to_string()),
    }
}

/// The checkpoint picker: numbered 1-9, ↑↓ + digits + Enter + Esc, with
/// the per-session cost shown. Without a controllable terminal it prints
/// the list and points at `amparo run --resume`.
async fn run_picker(
    ui: &Arc<Ui>,
    _live: &Arc<Mutex<Option<Live>>>,
    main_rx: &mut mpsc::UnboundedReceiver<ReaderMsg>,
) -> Option<Checkpoint> {
    let workspace_root = PathPolicy::from_env().workspace_root;
    let store = JsonCheckpointStore::new(&workspace_root);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let list: Vec<Checkpoint> = store
        .list_incomplete("cli")
        .into_iter()
        .filter(|c| now.saturating_sub(c.started_at) <= STALE_CHECKPOINT_SECS)
        .take(9)
        .collect();
    if list.is_empty() {
        ui.line("[session] nothing to resume — no running checkpoints");
        return None;
    }
    let paint = ui.paint();
    let rate = ui.rate();
    if !ui.controls() {
        for (i, c) in list.iter().enumerate() {
            ui.line(&picker_row(
                &paint,
                rate,
                i,
                &c.task_id,
                &c.prompt,
                c.started_at,
                false,
            ));
        }
        ui.line("[session] the picker needs a terminal — `amparo run --resume` resumes the newest");
        return None;
    }
    let rows: Vec<(String, String, u64)> = list
        .iter()
        .map(|c| (c.task_id.clone(), c.prompt.clone(), c.started_at))
        .collect();
    ui.set_mode(Mode::Picker);
    ui.set_title("amparo · resume");
    let height = rows.len() + 1;
    ui.picker_begin(&picker_screen(&paint, rate, &rows, 0));
    let mut sel = 0usize;
    loop {
        let Some(msg) = main_rx.recv().await else {
            ui.picker_end();
            return None;
        };
        match msg {
            ReaderMsg::Pick(PickKey::Up) if sel > 0 => {
                sel -= 1;
                ui.picker_redraw(&picker_screen(&paint, rate, &rows, sel), height);
            }
            ReaderMsg::Pick(PickKey::Down) if sel + 1 < rows.len() => {
                sel += 1;
                ui.picker_redraw(&picker_screen(&paint, rate, &rows, sel), height);
            }
            ReaderMsg::Pick(PickKey::Char(c)) if c.is_ascii_digit() => {
                let n = (c as u8 - b'0') as usize;
                if n >= 1 && n <= rows.len() {
                    ui.picker_end();
                    return Some(list[n - 1].clone());
                }
            }
            ReaderMsg::Pick(PickKey::Enter) => {
                ui.picker_end();
                return Some(list[sel].clone());
            }
            ReaderMsg::Pick(PickKey::Esc) | ReaderMsg::Cancel | ReaderMsg::Quit => {
                ui.picker_end();
                return None;
            }
            _ => {}
        }
    }
}

/// The interactive loop: idle prompt → task (cancelable) → prompt again,
/// with the live status refreshed each idle.
async fn run_interactive(
    ui: &Arc<Ui>,
    flags: &RunFlags,
    live: &Arc<Mutex<Option<Live>>>,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    mut main_rx: mpsc::UnboundedReceiver<ReaderMsg>,
    raw: &RawSlot,
) -> Result<(), String> {
    let mut failed = false;
    loop {
        let status = {
            let guard = live.lock().unwrap();
            let paint = ui.paint();
            match guard.as_ref() {
                Some(l) => {
                    let (p, m) = live_segments(l);
                    status_line(&paint, ui.width(), l.policy.audit_mode(), &p, &m, ui.cost())
                }
                None => status_line(&paint, ui.width(), false, "—", "—", ui.cost()),
            }
        };
        ui.set_status(status);
        ui.prompt_ready();
        let Some(msg) = main_rx.recv().await else {
            break;
        };
        match msg {
            ReaderMsg::Line(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                match handle_command(ui, live, raw, &line, &mut main_rx).await {
                    CommandOut::Quit => break,
                    CommandOut::Handled => {}
                    CommandOut::Task(task) => {
                        ui.task_begin();
                        match run_task_with_cancel(ui, flags, &task, None, live, keys, &mut main_rx)
                            .await
                        {
                            TaskOutcome::Exit => break,
                            TaskOutcome::Done(Err(_)) => failed = true,
                            TaskOutcome::Done(Ok(())) => {}
                        }
                        maybe_reprint_chain(ui, &live.lock().unwrap());
                    }
                    CommandOut::Resume(cp) => {
                        ui.task_begin();
                        match run_task_with_cancel(
                            ui,
                            flags,
                            "",
                            Some(&cp),
                            live,
                            keys,
                            &mut main_rx,
                        )
                        .await
                        {
                            TaskOutcome::Exit => break,
                            TaskOutcome::Done(Err(_)) => failed = true,
                            TaskOutcome::Done(Ok(())) => {}
                        }
                        maybe_reprint_chain(ui, &live.lock().unwrap());
                    }
                }
            }
            ReaderMsg::Cancel | ReaderMsg::Quit => break,
            ReaderMsg::Focus(true) => {
                if let Some(l) = ui.focus_gained() {
                    ui.line(&l);
                }
            }
            ReaderMsg::Focus(false) => {
                ui.focus_lost();
            }
            ReaderMsg::Pick(_) => {} // a stray picker key outside the picker
        }
    }
    if failed {
        Err("one or more tasks failed".to_string())
    } else {
        Ok(())
    }
}

/// The piped loop: one task per stdin line, zero escapes, approvals fail
/// closed. `/resume` prints the list and points at `amparo run --resume`.
async fn run_piped(
    ui: &Arc<Ui>,
    flags: &RunFlags,
    live: &Arc<Mutex<Option<Live>>>,
    keys: &Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>>,
    main_rx: &mut mpsc::UnboundedReceiver<ReaderMsg>,
    raw: &RawSlot,
) -> Result<(), String> {
    let mut failed = false;
    // The cancel watch stays armed for the whole session; piped runs are
    // never canceled (EOF ends them).
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match handle_command(ui, live, raw, &line, main_rx).await {
            CommandOut::Quit => break,
            CommandOut::Handled => {}
            CommandOut::Task(task) => {
                if run_one_task(ui, flags, &task, None, live, keys, cancel_rx.clone())
                    .await
                    .is_err()
                {
                    failed = true;
                }
                maybe_reprint_chain(ui, &live.lock().unwrap());
            }
            CommandOut::Resume(cp) => {
                if run_one_task(ui, flags, "", Some(&cp), live, keys, cancel_rx.clone())
                    .await
                    .is_err()
                {
                    failed = true;
                }
                maybe_reprint_chain(ui, &live.lock().unwrap());
            }
        }
    }
    if failed {
        Err("one or more tasks failed".to_string())
    } else {
        Ok(())
    }
}

// ────────────────────────────────────────────────────────── run_tui ──

/// The whole surface, both shapes: interactive (raw mode, reader thread,
/// keypress approvals) or piped (one task per line, plain).
async fn run_tui(flags: RunFlags) -> Result<(), String> {
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    #[cfg(unix)]
    let controls = tty && !dumb;
    #[cfg(not(unix))]
    let controls = false;
    let paint = Paint::detect(controls);
    let width = if controls {
        term_width().unwrap_or(80)
    } else {
        80
    };
    let ui = Arc::new(Ui::new(paint, controls, width));
    let _ = UI.set(Arc::clone(&ui));

    run::apply_workspace(&flags);

    // First-run wizard (#167): an interactive boot with no LLM configured
    // falls into the wizard instead of the prompt. Piped mode never starts
    // it — the wizard would eat the task stream.
    if controls {
        crate::wizard::ensure_configured()?;
    }

    let (main_tx, main_rx) = mpsc::unbounded_channel::<ReaderMsg>();
    #[cfg(not(unix))]
    let keys: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    #[cfg(unix)]
    let keys: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<char>>>> = {
        let (key_tx, key_rx) = mpsc::unbounded_channel::<char>();
        if controls {
            let reader_ui = Arc::clone(&ui);
            std::thread::spawn(move || reader_thread(reader_ui, main_tx.clone(), key_tx));
        }
        Arc::new(tokio::sync::Mutex::new(Some(key_rx)))
    };
    #[cfg(not(unix))]
    let _main_tx = main_tx; // no reader thread here — stdin stays cooked

    // Raw mode for the life of the run: the guard restores the terminal
    // on drop. The `!` escape borrows the slot — a child gets a cooked
    // terminal, then raw mode re-enters.
    let raw = RawSlot::new();
    #[cfg(unix)]
    if controls {
        ui.enable_modes();
        ui.set_title("amparo · idle");
        raw.set(enter_raw_mode());
    }

    let live: Arc<Mutex<Option<Live>>> = Arc::new(Mutex::new(None));
    boot_wire(&ui, &flags, &live, &keys).await?;

    let ticker_ui = Arc::clone(&ui);
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            ticker_ui.think_tick();
        }
    });

    let result = if controls {
        run_interactive(&ui, &flags, &live, &keys, main_rx, &raw).await
    } else {
        drop(main_rx);
        let (_, mut noop_rx) = mpsc::unbounded_channel::<ReaderMsg>();
        run_piped(&ui, &flags, &live, &keys, &mut noop_rx, &raw).await
    };
    ticker.abort();
    ui.shutdown();
    result
}

// ───────────────────────────────────────────────────────────── flags ──

/// The task placeholder that lets the run parser accept a taskless
/// invocation, stripped before the wire.
const TASK_SENTINEL: &str = "__amparo_tui_sentinel__";

/// Parse outcomes, mirroring the run parser's shape.
enum ParseTuiResult {
    Run(RunFlags),
    Help,
    Error(String),
}

/// Parses `amparo tui` flags: the `amparo run` set minus the approval
/// wiring, which the TUI replaces with itself.
fn parse_tui_flags(args: impl Iterator<Item = String>) -> ParseTuiResult {
    let args: Vec<String> = args.collect();
    // TUI-specific rejections first, so the messages speak the TUI's
    // grammar rather than the run parser's.
    for a in &args {
        match a.as_str() {
            "--auto-approve" => {
                return ParseTuiResult::Error(
                    "--auto-approve has no place in `amparo tui` — you are the human gate"
                        .to_string(),
                )
            }
            "--auto-deny" => {
                return ParseTuiResult::Error(
                    "--auto-deny has no place in `amparo tui` — you are the human gate".to_string(),
                )
            }
            "--approval-endpoint" => {
                return ParseTuiResult::Error(
                    "--approval-endpoint has no place in `amparo tui` — approvals render here, not on a server"
                        .to_string(),
                )
            }
            "--resume" => {
                return ParseTuiResult::Error(
                    "--resume is the picker's job in `amparo tui` — type /resume at the prompt"
                        .to_string(),
                )
            }
            _ => {}
        }
    }
    match run::parse_run_flags(
        args.into_iter()
            .chain(std::iter::once(TASK_SENTINEL.to_string())),
    ) {
        run::ParseRunResult::Help => ParseTuiResult::Help,
        run::ParseRunResult::Error(m) => ParseTuiResult::Error(m),
        run::ParseRunResult::Run(mut flags) => {
            if flags.task != TASK_SENTINEL {
                return ParseTuiResult::Error(
                    "amparo tui takes no task — type it at the prompt".to_string(),
                );
            }
            flags.task.clear();
            ParseTuiResult::Run(flags)
        }
    }
}

/// The `amparo tui --help` copy.
const TUI_USAGE: &str = "\
amparo tui — the interactive terminal surface

USAGE:
  amparo tui [FLAGS]

FLAGS (the `amparo run` set, minus the approval wiring):
  --policy-url URL         wire a policy engine (Guardrail wire protocol)
  --allow-all              opt out of policy (local experiments)
  --trust-ceiling TIER     observational | local_mutating |
                           external_effector | system_control
  --max-steps N            cap loop iterations
  --model MODEL            override AMPARO_INFERENCE_MODEL
  --timeout SECS           inference timeout
  --workspace DIR          workspace root
  --growth / --no-growth   lab notebook on/off
  --ledger-max-bytes SIZE  privacy ledger quota
  --max-sub-agents N       swarm budget (0 = off)
  --session-id ID          policy-engine session tag
  --webhook-url URL        send_notification transport
  --help                   this help

The TUI is the human gate: --auto-approve, --auto-deny,
--approval-endpoint and --resume are rejected here.

PIPED: `echo \"task\" | amparo tui` runs one task per stdin line
(zero escapes; approvals fail closed). /resume needs a terminal.";

/// The `amparo tui` entry point — parse, then run the surface.
pub(crate) async fn dispatch(args: impl Iterator<Item = String>) {
    match parse_tui_flags(args) {
        ParseTuiResult::Help => println!("{TUI_USAGE}"),
        ParseTuiResult::Error(m) => {
            eprintln!("{m}");
            std::process::exit(2);
        }
        ParseTuiResult::Run(flags) => {
            if let Err(m) = run_tui(flags).await {
                eprintln!("amparo tui: {m}");
                std::process::exit(1);
            }
        }
    }
}

// ─────────────────────────────────────────────────── terminal control ──

#[cfg(unix)]
fn term_width() -> Option<usize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }
}

#[cfg(not(unix))]
fn term_width() -> Option<usize> {
    None
}

/// Restores the original terminal settings on drop.
#[cfg(unix)]
struct RawGuard {
    orig: libc::termios,
}

/// Enters raw mode for the reader: no canonical line, no echo, no
/// signals, no flow control — but output processing stays, and reads
/// return within 0.1s so a lone ESC resolves as the Esc key.
#[cfg(unix)]
fn enter_raw_mode() -> Option<RawGuard> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 1;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        Some(RawGuard { orig })
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
    }
}

/// The raw-mode slot the `!` escape borrows: the child runs on a cooked
/// terminal, and raw mode re-enters afterwards. Empty in piped mode and
/// on non-unix platforms (no raw mode there at all).
struct RawSlot {
    #[cfg(unix)]
    guard: Mutex<Option<RawGuard>>,
}

impl RawSlot {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            guard: Mutex::new(None),
        }
    }

    /// Hands the fresh guard from [`enter_raw_mode`] to the slot (run
    /// start).
    #[cfg(unix)]
    fn set(&self, guard: Option<RawGuard>) {
        *self.guard.lock().unwrap() = guard;
    }

    /// Drops the guard for the child — the terminal returns to cooked
    /// mode (the drop restores the original settings).
    #[cfg(unix)]
    fn drop_for_child(&self) {
        self.guard.lock().unwrap().take();
    }

    /// Re-enters raw mode after the child.
    #[cfg(unix)]
    fn reenter(&self) {
        *self.guard.lock().unwrap() = enter_raw_mode();
    }
}

#[cfg(unix)]
use std::io::Read;

/// One byte, or `None` on a read timeout (VMIN=1, VTIME=1).
#[cfg(unix)]
fn read_byte(stdin: &mut std::io::Stdin) -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        match stdin.read(&mut b) {
            Ok(0) => return None,
            Ok(_) => return Some(b[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

/// How many bytes the UTF-8 sequence starting with `first` needs.
#[cfg(unix)]
fn utf8_len(first: u8) -> usize {
    if first & 0x80 == 0 {
        1
    } else if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        3
    } else if first & 0xF8 == 0xF0 {
        4
    } else {
        1 // an invalid lead byte is consumed alone
    }
}

/// Accumulates UTF-8 bytes, flushing each complete character into `out`.
#[cfg(unix)]
fn push_utf8(pending: &mut Vec<u8>, out: &mut String, b: u8) {
    pending.push(b);
    let needed = utf8_len(pending[0]);
    if pending.len() < needed {
        return;
    }
    out.push_str(&String::from_utf8_lossy(pending));
    pending.clear();
}

/// Decodes one byte into the reader's edit, buffering partial UTF-8.
#[cfg(unix)]
fn push_utf8_edit(pending: &mut Vec<u8>, ui: &Ui, b: u8) {
    pending.push(b);
    let needed = utf8_len(pending[0]);
    if pending.len() < needed {
        return;
    }
    let s = String::from_utf8_lossy(pending).into_owned();
    pending.clear();
    for c in s.chars() {
        ui.edit(EditOp::Char(c));
    }
}

/// The reader messages the main loop acts on.
enum ReaderMsg {
    Line(String),
    Cancel,
    Quit,
    Focus(bool),
    Pick(PickKey),
}

/// Decoded picker keys.
enum PickKey {
    Up,
    Down,
    Enter,
    Esc,
    Char(char),
}

/// The raw-mode byte reader: routes by surface mode — approval keys to the
/// gate, picker keys to the picker, everything else into the input line
/// with history, focus events and bracketed paste.
#[cfg(unix)]
fn reader_thread(
    ui: Arc<Ui>,
    main: mpsc::UnboundedSender<ReaderMsg>,
    keys: mpsc::UnboundedSender<char>,
) {
    let mut stdin = std::io::stdin();
    let mut history: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut history_pos: Option<usize> = None;
    let mut stash: Option<String> = None;
    let mut paste = false;
    let mut paste_buf = String::new();
    let mut esc = false;
    let mut in_csi = false;
    let mut csi_params: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();

    loop {
        let Some(b) = read_byte(&mut stdin) else {
            // A lone ESC resolves as the Esc key.
            if esc {
                esc = false;
                csi_params.clear();
                if ui.mode() == Mode::Picker {
                    let _ = main.send(ReaderMsg::Pick(PickKey::Esc));
                } else {
                    ui.edit(EditOp::Clear);
                }
            }
            continue;
        };

        if paste {
            if b == 0x1b {
                // Possibly the bracketed-paste terminator ESC[201~.
                let mut seq = Vec::new();
                let mut saw_open = false;
                let mut closed = false;
                loop {
                    let Some(n) = read_byte(&mut stdin) else {
                        break;
                    };
                    seq.push(n);
                    if !saw_open {
                        if n == b'[' {
                            saw_open = true;
                        } else {
                            break;
                        }
                    } else if (0x40..=0x7e).contains(&n) {
                        closed = true;
                        break;
                    }
                    if seq.len() > 8 {
                        break;
                    }
                }
                if closed && seq == b"[201~" {
                    paste = false;
                    if !pending.is_empty() {
                        paste_buf.push_str(&String::from_utf8_lossy(&pending));
                        pending.clear();
                    }
                    paste_buf = paste_buf.replace('\r', "\n");
                    if !paste_buf.is_empty() {
                        ui.edit(EditOp::Append(std::mem::take(&mut paste_buf)));
                    }
                    continue;
                }
                // A literal ESC inside the pasted text.
                paste_buf.push(0x1b as char);
                paste_buf.push_str(&String::from_utf8_lossy(&seq));
            } else if b == b'\r' {
                if !pending.is_empty() {
                    paste_buf.push_str(&String::from_utf8_lossy(&pending));
                    pending.clear();
                }
                paste_buf.push('\n');
            } else {
                push_utf8(&mut pending, &mut paste_buf, b);
            }
            continue;
        }

        if esc {
            esc = false;
            if b == b'[' {
                in_csi = true;
                csi_params.clear();
                continue;
            }
            // ESC followed by a non-[ byte: the ESC acts as Esc, then the
            // byte is handled on its own.
            if ui.mode() == Mode::Picker {
                let _ = main.send(ReaderMsg::Pick(PickKey::Esc));
            } else {
                ui.edit(EditOp::Clear);
            }
        }

        if in_csi {
            csi_params.push(b);
            if (0x40..=0x7e).contains(&b) {
                in_csi = false;
                let params = std::mem::take(&mut csi_params);
                match params.as_slice() {
                    b"A" => {
                        if ui.mode() == Mode::Picker {
                            let _ = main.send(ReaderMsg::Pick(PickKey::Up));
                            continue;
                        }
                        // History up.
                        if history.is_empty() {
                            continue;
                        }
                        let next = history_pos.map(|p| p + 1).unwrap_or(0);
                        if next < history.len() {
                            if history_pos.is_none() {
                                stash = Some(ui.input());
                            }
                            history_pos = Some(next);
                            let line = history[history.len() - 1 - next].clone();
                            ui.edit(EditOp::Set(line));
                        }
                    }
                    b"B" => {
                        if ui.mode() == Mode::Picker {
                            let _ = main.send(ReaderMsg::Pick(PickKey::Down));
                            continue;
                        }
                        // History down.
                        match history_pos {
                            Some(0) => {
                                history_pos = None;
                                ui.edit(EditOp::Set(stash.take().unwrap_or_default()));
                            }
                            Some(p) => {
                                let np = p - 1;
                                history_pos = Some(np);
                                let line = history[history.len() - 1 - np].clone();
                                ui.edit(EditOp::Set(line));
                            }
                            None => {}
                        }
                    }
                    b"I" => {
                        let _ = main.send(ReaderMsg::Focus(true));
                    }
                    b"O" => {
                        let _ = main.send(ReaderMsg::Focus(false));
                    }
                    b"200~" => {
                        paste = true;
                        paste_buf.clear();
                    }
                    _ => {}
                }
            }
            continue;
        }

        if b == 0x1b {
            esc = true;
            continue;
        }

        match ui.mode() {
            Mode::Approval => {
                let _ = keys.send(if b == 0x03 { 'n' } else { b as char });
            }
            Mode::Picker => match b {
                b'\r' | b'\n' => {
                    let _ = main.send(ReaderMsg::Pick(PickKey::Enter));
                }
                0x03 | 0x04 => {
                    let _ = main.send(ReaderMsg::Pick(PickKey::Esc));
                }
                c if c.is_ascii() && !c.is_ascii_control() => {
                    let _ = main.send(ReaderMsg::Pick(PickKey::Char(c as char)));
                }
                _ => {}
            },
            Mode::Normal => {
                if !ui.prompt_live() {
                    // A task is running: everything holds except cancel.
                    if b == 0x03 {
                        let _ = main.send(ReaderMsg::Cancel);
                    }
                    continue;
                }
                match b {
                    0x03 => {
                        if ui.task_active() {
                            let _ = main.send(ReaderMsg::Cancel);
                        } else {
                            let _ = main.send(ReaderMsg::Quit);
                        }
                    }
                    0x04 => {
                        if ui.input().is_empty() {
                            let _ = main.send(ReaderMsg::Quit);
                        }
                    }
                    b'\r' | b'\n' => {
                        let line = ui.submit_line();
                        if !line.trim().is_empty() {
                            if history.len() == 100 {
                                history.pop_front();
                            }
                            history.push_back(line.clone());
                        }
                        history_pos = None;
                        stash = None;
                        let _ = main.send(ReaderMsg::Line(line));
                    }
                    0x7f | 0x08 => {
                        ui.edit(EditOp::Backspace);
                        history_pos = None;
                    }
                    _ => {
                        push_utf8_edit(&mut pending, &ui, b);
                    }
                }
            }
            _ => {}
        }
    }
}

// ───────────────────────────────────────────────────────────── tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Paint {
        Paint::with_colors(false)
    }

    fn colored() -> Paint {
        Paint::with_colors(true)
    }

    fn args(list: &[&str]) -> std::vec::IntoIter<String> {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn banner_info() -> BannerInfo {
        BannerInfo {
            chain: "test".to_string(),
            tool_count: 14,
            ceiling: "system_control".to_string(),
            policy: "deny-all (no --policy-url)".to_string(),
            approval: "you".to_string(),
            infer: "mock".to_string(),
            memory: "built-in store".to_string(),
        }
    }

    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "edit_file".to_string(),
            arguments: serde_json::json!({"path": "src/hints.rs"}),
            reasons: vec!["policy engine escalation: destructive-class write".to_string()],
            blast_radius: None,
            session_label: Some("sess-481".to_string()),
            rollback: None,
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn paint_plain_applies_nothing_and_colored_uses_the_palette() {
        assert_eq!(plain().apply(Ink::Bad, "x"), "x");
        let s = colored().apply(Ink::Bad, "x");
        assert!(s.contains("\x1b[38;2;239;68;68m"), "{s}");
        let s = colored().apply(Ink::Dim, "x");
        assert!(s.contains("2;38;2;148;163;184"), "{s}");
        let s = colored().apply(Ink::WarnBold, "x");
        assert!(s.contains("1;38;2;245;158;11"), "{s}");
        let s = colored().apply(Ink::ItalicDim, "x");
        assert!(s.contains("3;2;38;2;148;163;184"), "{s}");
    }

    #[test]
    fn render_chain_maps_states_to_symbols_compact_and_expanded() {
        assert_eq!(
            render_chain(
                [
                    LinkState::Pass,
                    LinkState::Pass,
                    LinkState::Escalate,
                    LinkState::Pending
                ],
                false,
                &plain()
            ),
            "◆▲§◉"
        );
        assert_eq!(
            render_chain(
                [
                    LinkState::Pass,
                    LinkState::Pass,
                    LinkState::Escalate,
                    LinkState::Pending
                ],
                true,
                &plain()
            ),
            "◆──▲──§──◉"
        );
        // Denied renders red, unreached renders dim, pending bright amber.
        let s = render_chain(
            [
                LinkState::Denied,
                LinkState::Unreached,
                LinkState::Pending,
                LinkState::Pass,
            ],
            false,
            &colored(),
        );
        assert!(s.contains("38;2;239;68;68"), "{s}");
        assert!(s.contains("2;38;2;148;163;184"), "{s}");
        assert!(s.contains("1;38;2;245;158;11"), "{s}");
        assert!(s.contains("38;2;16;185;129"), "{s}");
    }

    #[test]
    fn gate_states_map_every_known_decision() {
        use LinkState::*;
        assert_eq!(
            gate_states("trust_blocked", false),
            Some([Pass, Denied, Unreached, Unreached])
        );
        assert_eq!(
            gate_states("policy_denied", false),
            Some([Pass, Pass, Denied, Unreached])
        );
        assert_eq!(
            gate_states("approval_denied", true),
            Some([Pass, Pass, Escalate, Denied])
        );
        assert_eq!(
            gate_states("approval_denied", false),
            Some([Pass, Pass, Pass, Denied])
        );
        assert_eq!(
            gate_states("unknown_tool", false),
            Some([Denied, Unreached, Unreached, Unreached])
        );
        assert_eq!(gate_states("allowed", false), None);
        // A future decision falls through to the plain [gate] line.
        assert_eq!(gate_states("some_future_verdict", false), None);
    }

    #[test]
    fn gate_suffixes_carry_the_honest_short_reason() {
        let reasons = vec!["destructive flags".to_string()];
        assert_eq!(gate_suffix("trust_blocked", &reasons), "trust ceiling");
        assert_eq!(
            gate_suffix("policy_denied", &reasons),
            "policy: destructive flags"
        );
        assert_eq!(gate_suffix("approval_denied", &reasons), "you denied");
        assert_eq!(gate_suffix("unknown_tool", &reasons), "not registered");
    }

    #[test]
    fn approval_card_carries_the_full_copy() {
        let lines = approval_card_body(&sample_request(), &plain());
        let card = lines.join("\n");
        assert!(
            card.contains("▐ approval required — edit_file src/hints.rs"),
            "{card}"
        );
        assert!(card.contains("◆──▲──§──◉"), "{card}");
        assert!(card.contains("policy escalates → human decides"), "{card}");
        assert!(
            card.contains("why:      policy engine escalation: destructive-class write"),
            "{card}"
        );
        assert!(card.contains("blast:    not classified"), "{card}");
        assert!(card.contains("rollback: none declared"), "{card}");
        assert!(card.contains("who:      sess-481"), "{card}");
    }

    #[test]
    fn approval_card_distinguishes_tier_and_policy_escalation() {
        let mut tier_only = sample_request();
        tier_only.reasons = vec!["tier external_effector above the ceiling".to_string()];
        let card = approval_card_body(&tier_only, &plain()).join("\n");
        assert!(card.contains("its tier asks → human decides"), "{card}");

        let mut with_rollback = sample_request();
        with_rollback.rollback = Some(amparo_tools::RollbackSpec {
            undo: "restore the file from backup".to_string(),
            markers: vec!["src/hints.rs.bak".to_string()],
        });
        let card = approval_card_body(&with_rollback, &plain()).join("\n");
        assert!(
            card.contains("rollback: restore the file from backup (backup: src/hints.rs.bak)"),
            "{card}"
        );
    }

    #[test]
    fn meter_bar_fills_in_proportion_and_meter_reads_the_copy() {
        let (filled, empty) = meter_bar(30, 60, 20);
        assert_eq!(filled.chars().count(), 10);
        assert_eq!(empty.chars().count(), 10);
        let (filled, empty) = meter_bar(60, 60, 20);
        assert_eq!(filled.chars().count(), 20);
        assert_eq!(empty.chars().count(), 0);
        let (filled, _) = meter_bar(0, 60, 20);
        assert_eq!(filled.chars().count(), 0);
        let m = approval_meter(47, &plain());
        assert!(m.contains("approve? [y/N]"), "{m}");
        assert!(m.contains("47s"), "{m}");
        assert!(m.contains("── deny on timeout"), "{m}");
    }

    #[test]
    fn picker_rows_number_mark_and_cost() {
        let p = plain();
        let row = picker_row(
            &p,
            Some(2.0),
            0,
            "sess-481",
            "tighten the rollback hints",
            now_secs() - 7200,
            true,
        );
        assert!(row.starts_with("› 1"), "{row}");
        assert!(row.contains("sess-481"), "{row}");
        assert!(row.contains("tighten the rollback hints"), "{row}");
        assert!(row.contains("2h ago"), "{row}");
        assert!(row.contains("~$"), "{row}");
        // No rate, no cost column.
        let row = picker_row(&p, None, 0, "sess-481", "x", now_secs(), false);
        assert!(!row.contains('~'), "{row}");
        assert!(row.starts_with(' '), "{row}");
    }

    #[test]
    fn picker_cost_estimates_from_the_conversation() {
        let conv = vec![amparo_inference::ChatMessage::user("hello world")];
        assert_eq!(picker_cost("prompt", &conv, None), None);
        let cost = picker_cost("prompt", &conv, Some(2.0));
        assert!(cost.unwrap().starts_with("~$"),);
    }

    #[test]
    fn chain_line_registers_each_policy_mode() {
        let p = plain();
        let mut info = banner_info();
        info.policy = "deny-all (no --policy-url)".to_string();
        let l = chain_line(&info, false, &p);
        assert!(l.contains("◆ registry: 14 tools"), "{l}");
        assert!(l.contains("▲ ceiling: system_control"), "{l}");
        assert!(l.contains("built-in (deny-all default)"), "{l}");
        info.policy = "wire https://example.com (audit)".to_string();
        let l = chain_line(&info, false, &p);
        assert!(
            l.contains("wire https://example.com (audit) (enforce)"),
            "{l}"
        );
        info.policy = "allow-all".to_string();
        let l = chain_line(&info, false, &p);
        assert!(l.contains("built-in (allow-all — operator opt-in)"), "{l}");
        // Audit recolors the segment amber.
        let l = chain_line(&banner_info(), true, &colored());
        assert!(l.contains("audit — verdicts advisory"), "{l}");
        assert!(l.contains("38;2;245;158;11"), "{l}");
    }

    #[test]
    fn status_line_drops_below_72_columns_and_ambers_audit() {
        assert_eq!(
            status_line(&plain(), 71, false, "enforce", "engram", None),
            None
        );
        let s = status_line(&plain(), 80, false, "enforce", "engram", Some(0.04)).unwrap();
        assert!(s.contains("◆▲§◉ · enforce · engram · ~$0.04"), "{s}");
        let s = status_line(&colored(), 80, true, "audit", "engram", None).unwrap();
        assert!(s.contains("38;2;245;158;11"), "{s}");
    }

    #[test]
    fn hairline_keeps_the_run_marker_and_segments() {
        let p = plain();
        let l = hairline(
            &p,
            80,
            "sess-481",
            &("enforce".to_string(), "engram".to_string()),
        );
        assert!(l.starts_with("──── sess-481 "), "{l}");
        assert!(l.ends_with(" enforce · engram ──"), "{l}");
        assert_eq!(l.chars().count(), 80);
        // Empty segments degrade to a bare rule.
        let l = hairline(&p, 80, "sess-481", &(String::new(), String::new()));
        assert!(l.ends_with("──"), "{l}");
    }

    #[test]
    fn banner_is_plain_and_names_every_section() {
        let lines = banner_lines(&banner_info(), &plain());
        let banner = lines.join("\n");
        assert!(!banner.contains("\x1b["), "piped banners carry no escapes");
        assert!(banner.contains("Greetings! My name is Amparo"), "{banner}");
        assert!(banner.contains("[wake] Amparo is awake."), "{banner}");
        assert!(
            banner.contains("Every action I take passes one chain"),
            "{banner}"
        );
        assert!(banner.contains("──▶"), "{banner}");
        assert!(
            banner.contains("Deny by default. Fail closed at every link."),
            "{banner}"
        );
        assert!(
            banner.contains("[key]   pass · escalate · denied · not reached"),
            "{banner}"
        );
        assert!(banner.contains("[chain]"), "{banner}");
        assert!(banner.contains("[infer] mock"), "{banner}");
        assert!(banner.contains("[memory] built-in store"), "{banner}");
    }

    #[test]
    fn key_line_gains_the_explainer_when_full() {
        assert!(!key_line(&plain(), false).contains("light up"));
        assert!(key_line(&plain(), true).contains("the symbols light up as each verdict arrives"));
    }

    #[test]
    fn policy_block_reports_the_builtin_default_and_recent_verdicts() {
        let p = plain();
        let block = policy_block(&p, false, None, &[]);
        let s = block.join("\n");
        assert!(s.contains("── policy "), "{s}");
        assert!(s.contains("built-in · deny-all default"), "{s}");
        assert!(s.contains("nothing judged yet"), "{s}");
        let recent = vec![(
            "policy_denied".to_string(),
            "run_command rm -rf".to_string(),
            now_secs() - 41,
        )];
        let block = policy_block(&p, false, None, &recent);
        let s = block.join("\n");
        assert!(s.contains("§ deny"), "{s}");
        assert!(s.contains("41s ago"), "{s}");
        assert!(
            s.contains("connect a Guardrail engine — Guardrail Console-managed policies"),
            "{s}"
        );
    }

    #[test]
    fn memory_block_reports_backend_and_source() {
        let p = plain();
        let s = memory_block(&p, false, None).join("\n");
        assert!(s.contains("── memory "), "{s}");
        assert!(s.contains("built-in store"), "{s}");
        assert!(s.contains("not answering"), "{s}");
        assert!(
            s.contains("connect Engram Vault for searchable memory"),
            "{s}"
        );
        assert!(
            s.contains("/memory add still works — the built-in store keeps this session only"),
            "{s}"
        );
    }

    #[test]
    fn host_of_keeps_the_port_and_drops_the_path() {
        assert_eq!(
            host_of("https://guardrail.elai-intelligence.com"),
            Some("guardrail.elai-intelligence.com".to_string())
        );
        assert_eq!(
            host_of("https://guardrail.elai-intelligence.com/api/upstream"),
            Some("guardrail.elai-intelligence.com".to_string())
        );
        assert_eq!(
            host_of("http://127.0.0.1:47800"),
            Some("127.0.0.1:47800".to_string())
        );
        assert_eq!(
            host_of("http://127.0.0.1:47800?q=1"),
            Some("127.0.0.1:47800".to_string())
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn banner_policy_host_parses_wire_lines_only() {
        assert_eq!(
            banner_policy_host("wire http://127.0.0.1:47800"),
            Some("127.0.0.1:47800".to_string())
        );
        assert_eq!(
            banner_policy_host("wire http://127.0.0.1:47800 (profile)"),
            Some("127.0.0.1:47800".to_string())
        );
        assert_eq!(
            banner_policy_host("deny-all (no --policy-url or --allow-all)"),
            None
        );
        assert_eq!(banner_policy_host("wire not a url"), None);
    }

    #[test]
    fn display_arg_renders_single_key_and_compact_json() {
        assert_eq!(
            display_arg(&serde_json::json!({"path": "README.md"})),
            "README.md"
        );
        let s = display_arg(&serde_json::json!({"path": "a", "mode": "w"}));
        assert!(
            s.contains("\"path\":\"a\"") || s.contains("\"path\": \"a\""),
            "{s}"
        );
        assert_eq!(display_arg(&serde_json::json!("x")), "\"x\"");
    }

    #[test]
    fn clip_and_plain_len_behave() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
        let painted = colored().apply(Ink::Dim, "abc");
        assert_eq!(plain_len(&painted), 3);
    }

    #[test]
    fn ago_formats_each_scale() {
        let now = now_secs();
        assert_eq!(ago(now), "0s ago");
        assert_eq!(ago(now - 120), "2m ago");
        assert_eq!(ago(now - 7200), "2h ago");
        assert_eq!(ago(now - 172800), "2d ago");
    }

    #[test]
    fn parse_tui_flags_rejects_the_approval_wiring_with_tui_copy() {
        let cases = [
            ("--auto-approve", "human gate"),
            ("--auto-deny", "human gate"),
            ("--approval-endpoint", "approvals render here"),
            ("--resume", "picker"),
        ];
        for (flag, needle) in cases {
            match parse_tui_flags(args(&[flag])) {
                ParseTuiResult::Error(m) => assert!(m.contains(needle), "{flag}: {m}"),
                other => panic!("{flag} accepted: {:?}", variant(&other)),
            }
        }
    }

    #[test]
    fn parse_tui_flags_rejects_a_positional_task_and_accepts_run_flags() {
        match parse_tui_flags(args(&["hello"])) {
            ParseTuiResult::Error(m) => assert!(m.contains("takes no task"), "{m}"),
            other => panic!("positional accepted: {:?}", variant(&other)),
        }
        match parse_tui_flags(args(&["--trust-ceiling", "local_mutating", "--allow-all"])) {
            ParseTuiResult::Run(flags) => {
                assert!(flags.task.is_empty());
                assert_eq!(
                    flags.trust_ceiling,
                    amparo_tools::ToolTrustTier::LocalMutating
                );
                assert!(flags.allow_all);
            }
            other => panic!("run flags rejected: {:?}", variant(&other)),
        }
        match parse_tui_flags(args(&["--help"])) {
            ParseTuiResult::Help => {}
            other => panic!("--help rejected: {:?}", variant(&other)),
        }
    }

    #[test]
    fn tui_usage_names_the_surface_and_the_rejections() {
        assert!(TUI_USAGE.contains("amparo tui — the interactive terminal surface"));
        assert!(TUI_USAGE.contains("is the human gate"));
        assert!(TUI_USAGE.contains("zero escapes"));
    }

    fn variant(r: &ParseTuiResult) -> &'static str {
        match r {
            ParseTuiResult::Run(_) => "Run",
            ParseTuiResult::Help => "Help",
            ParseTuiResult::Error(_) => "Error",
        }
    }
}
