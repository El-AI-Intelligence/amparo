//! `amparo code` — the coding-terminal surface (M13).
//!
//! An alternate-screen file tree, greenfield hand-rolled ANSI — unlike the
//! TUI, which prints inline and scrolls like a log, this surface owns the
//! whole screen: a tree pane on the left (the workspace's git marks riding
//! each file), a reading pane on the right, and a status bar. The tree is
//! built by a plain recursive walk that skips `.git`/`target`/`node_modules`
//! and is bounded by a node budget; the git marks come from the tools
//! crate's [`GitStatusTool`] — the same status the agent sees. Selection
//! rides in inverse video, dirs collapse with `↵`, files open in the
//! reading pane.
//!
//! Editing (M13 W2): with a file open, `e` turns the status bar into a
//! one-line instruction prompt. Submitting it runs one agent turn through
//! the *same* gate chain as `amparo run` ([`crate::run::wire_with`] — the
//! profile, fail-closed inference, policy engine, approval). The surface's
//! own [`CodeApprovalGate`] parks each approval request in a shared slot
//! and the reader loop answers it with a single `y`/`n` press (deny-wins,
//! 60s fail-closed); while it waits, the right pane renders the proposal
//! as a diff — `edit_file` as `- old` / `+ new` lines, `patch_file` as the
//! patch verbatim, anything else as a small approval card. The write
//! executes through the tools' own [`PathPolicy`] and the policy engine —
//! no new write path, no policy bypass.
//!
//! **Degraded shapes** (all documented, all honest):
//! - *Piped* (`amparo code DIR < /dev/null`): a plain-text report — the
//!   tree plus the git summary, zero escapes. Editing needs a terminal;
//!   the report pretends to nothing else.
//! - *NO_COLOR*: the same surface without color (selection inverse stays —
//!   an attribute, not a color).
//! - *Windows*: no raw mode — the piped report prints, with a stderr hint
//!   when the operator asked for interactive.
//!
//! **Known simplifications, kept deliberate**: the walk is shallow-metadata
//! (symlinks never follow — no cycles); files cap at 1 MiB when opened and
//! binary files show a summary row instead of mojibake; resize is picked up
//! on the 0.1s read timeout, not by signal; the edit prompt edits with
//! backspace only (no cursor motion). Scientific voice, `[tag]` lines,
//! `—` in copy, no emoji.

#[cfg(unix)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::{Mutex as StdMutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use amparo_agent::{
    format_event, AgentEvent, ApprovalGate, ApprovalRequest, EventSink, TaskStatus,
};
use amparo_tools::git::GitStatusTool;
use amparo_tools::{PathPolicy, ToolCall, ToolExecutor};
#[cfg(unix)]
use amparo_tools::ToolTrustTier;
#[cfg(unix)]
use async_trait::async_trait;
use serde_json::json;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use tokio::sync::oneshot;

// Every new import above is `cfg(unix)` except the ones the piped report
// needs: the module-level `allow(dead_code, unused_variables)` on Windows
// does not cover unused imports.
#[cfg(unix)]
use crate::run::{self, RunFlags, Surface};
#[cfg(unix)]
use crate::raw::{enter_raw_mode, poll_byte, read_byte, term_size};

const CODE_USAGE: &str = "\
amparo code — the coding terminal

USAGE:
  amparo code [DIR]

ARGS:
  DIR   the tree root (default: the workspace root)

On a unix terminal this opens an alternate-screen file tree with the
workspace's git marks: ↑↓ move, ↵ open a file or toggle a directory,
r rescan, q quit. With a file open, e edits: type an instruction, ↵
submits it as one agent turn behind the same gate chain as
`amparo run`, and any proposed write is approved with a single y
(apply) or n (deny) press under a 60-second fail-closed deadline.
Piped, it prints a plain-text report — the tree plus the git summary,
zero escapes.";

/// The node budget for one tree build — a runaway directory (a mount
/// point, a cache) can never make the surface hang or the frame enormous.
const MAX_TREE_NODES: usize = 5000;

/// The most of a file the reading pane holds — the rest is truncated,
/// announced, not silently dropped.
const MAX_FILE_BYTES: usize = 1 << 20;

/// Directories the tree always skips. `.git` is a status source, not
/// content; `target`/`node_modules` are build caches.
const IGNORED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// How long an edit approval waits for a key before it denies itself —
/// the same fail-closed deadline as the TUI's gate.
#[cfg(unix)]
const EDIT_APPROVAL_SECS: u64 = 60;

/// The edit turn's event log keeps at most this many lines — the pane
/// holds a screen's worth; the cap bounds the memory, not the story.
#[cfg(unix)]
const EDIT_LOG_LINES: usize = 200;

/// One node of the tree — a directory with children, or a file.
#[derive(Debug)]
struct Node {
    name: String,
    is_dir: bool,
    /// Git mark from the workspace status: `M`/`A`/`D`/`R`/`?`.
    mark: Option<char>,
    children: Vec<Node>,
    /// The node budget ran out inside this subtree — the tree is
    /// incomplete, announced honestly.
    truncated: bool,
}

/// Builds the tree under `root`. Entries sort directories-first, then
/// alphabetical (case-insensitive) for a stable frame. `marks` keys are
/// absolute paths. `budget` is shared across the whole build and each
/// node costs one; exhausted, the walk stops and marks `truncated`.
fn build_tree(
    root: &Path,
    marks: &HashMap<PathBuf, char>,
    budget: &mut usize,
) -> std::io::Result<Node> {
    let mut node = Node {
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
        is_dir: true,
        mark: None,
        children: Vec::new(),
        truncated: false,
    };
    *budget = budget.saturating_sub(1);

    let mut entries = std::fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    // Dirs first (the same file_type predicate the walk below uses, so a
    // symlink-to-dir sorts where it renders), then case-insensitive alpha.
    entries.sort_by_key(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        (!e.file_type().map(|t| t.is_dir()).unwrap_or(false), name)
    });
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        if *budget == 0 {
            node.truncated = true;
            break;
        }
        if is_dir {
            let mut child_budget = *budget;
            match build_tree(&entry.path(), marks, &mut child_budget) {
                Ok(child) => {
                    node.children.push(child);
                    *budget = child_budget;
                }
                // An unreadable subtree is skipped, not fatal — the walk
                // reports the parts it could see.
                Err(_) => {}
            }
        } else {
            *budget = budget.saturating_sub(1);
            node.children.push(Node {
                name,
                is_dir: false,
                mark: marks.get(&entry.path()).copied(),
                children: Vec::new(),
                truncated: false,
            });
        }
    }
    Ok(node)
}

/// Counts files and directories in a built tree.
fn count(tree: &Node) -> (usize, usize) {
    let mut files = 0;
    let mut dirs = 1; // the root itself
    fn walk(n: &Node, files: &mut usize, dirs: &mut usize) {
        for c in &n.children {
            if c.is_dir {
                *dirs += 1;
                walk(c, files, dirs);
            } else {
                *files += 1;
            }
        }
    }
    walk(tree, &mut files, &mut dirs);
    (files, dirs)
}

/// True when any subtree reports truncation.
fn has_truncation(tree: &Node) -> bool {
    if tree.truncated {
        return true;
    }
    tree.children.iter().any(has_truncation)
}

/// Splits one `git status --porcelain -b` line into a mark and a path.
/// Renames resolve to the destination; quoted paths unescape.
fn porcelain_line(line: &str) -> Option<(char, String)> {
    if line.len() < 4 || line.starts_with("## ") {
        return None;
    }
    let status = &line[..2];
    let path = line[3..].trim_end();
    let mark = match status {
        "M " | " M" | "MM" | "AM" | "MA" => 'M',
        "A " | " A" => 'A',
        "D " | " D" => 'D',
        "R " | "RM" => 'R',
        "??" => '?',
        _ => return None,
    };
    // Renames: "R  old -> new" — the tree names the destination.
    let path = path.rsplit_once(" -> ").map(|(_, new)| new).unwrap_or(path);
    Some((mark, unquote_git_path(path)))
}

/// Strips git's C-style path quoting (used only when the name contains
/// spaces or escapes) — `"` quotes, `\\`, `\"`, `\t`, `\n`.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other), // \" \\ and the rest
            None => out.push('\\'),
        }
    }
    out
}

/// Runs the tools-crate git status against the workspace and folds the
/// porcelain output into absolute-path marks. Returns the marks plus the
/// branch (or `None` when the workspace is not a repository).
async fn git_marks(workspace: &Path) -> (HashMap<PathBuf, char>, Option<String>) {
    let policy = Arc::new(PathPolicy::from_root(workspace.to_path_buf()));
    let call = ToolCall {
        id: "code-git-status".into(),
        name: "git_status".into(),
        arguments: json!({}),
    };
    let res = GitStatusTool::with_policy(policy).execute(&call).await;
    if !res.success {
        return (HashMap::new(), None);
    }
    let branch = res.output["branch"].as_str().and_then(|b| {
        if b == "unknown" {
            None
        } else {
            Some(b.to_string())
        }
    });
    let mut marks = HashMap::new();
    if let Some(raw) = res.output["raw"].as_str() {
        for line in raw.lines() {
            if let Some((mark, path)) = porcelain_line(line) {
                marks.insert(workspace.join(path), mark);
            }
        }
    }
    (marks, branch)
}

/// The plain-text report — the piped / non-unix degradation. No escapes,
/// deterministic ordering.
fn render_report(root: &Path, tree: &Node, branch: Option<&str>) -> String {
    let (files, dirs) = count(tree);
    let mut out = String::new();
    out.push_str(&format!("amparo code — {}\n", root.display()));
    match branch {
        Some(b) => out.push_str(&format!("branch {b} · {files} files, {dirs} dirs\n")),
        None => out.push_str(&format!(
            "not a git repository — status hidden · {files} files, {dirs} dirs\n"
        )),
    }
    out.push('\n');
    fn rows(n: &Node, depth: usize, out: &mut String) {
        for c in &n.children {
            out.push_str(&"  ".repeat(depth));
            out.push_str(&c.name);
            if c.is_dir {
                out.push('/');
            } else if let Some(m) = c.mark {
                out.push_str(&format!("  {m}"));
            }
            out.push('\n');
            if c.is_dir {
                rows(c, depth + 1, out);
            }
        }
    }
    rows(tree, 0, &mut out);
    if has_truncation(tree) {
        out.push_str(&format!("… tree truncated at {MAX_TREE_NODES} nodes\n"));
    }
    out
}

/// Parses `amparo code` flags: one optional DIR, `--help`. The default
/// DIR is the workspace root (env, then `~/amparo-workspace`).
enum ParseCodeResult {
    Run(PathBuf),
    Help,
    Error(String),
}

fn parse_code_flags(args: impl Iterator<Item = String>) -> ParseCodeResult {
    let mut dir: Option<PathBuf> = None;
    for a in args {
        match a.as_str() {
            "--help" | "-h" => return ParseCodeResult::Help,
            _ if a.starts_with('-') => {
                return ParseCodeResult::Error(format!("unknown flag {a}; see `amparo code --help`"))
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            _ => {
                return ParseCodeResult::Error(
                    "one DIR at most; see `amparo code --help`".to_string(),
                )
            }
        }
    }
    let dir = dir.unwrap_or_else(|| PathPolicy::from_env().workspace_root);
    ParseCodeResult::Run(dir)
}

/// The `amparo code` entry point — parse, then report (piped) or open
/// the alternate-screen tree (unix terminal). Exit codes match the
/// surface contract: 0 help/report/clean quit, 2 usage, 1 failure.
pub(crate) async fn dispatch(args: impl Iterator<Item = String>) {
    match parse_code_flags(args) {
        ParseCodeResult::Help => println!("{CODE_USAGE}"),
        ParseCodeResult::Error(m) => {
            eprintln!("{m}");
            std::process::exit(2);
        }
        ParseCodeResult::Run(root) => {
            let workspace = PathPolicy::from_env().workspace_root;
            let (marks, branch) = git_marks(&workspace).await;
            let mut budget = MAX_TREE_NODES;
            let tree = match build_tree(&root, &marks, &mut budget) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("amparo code: {}: {e}", root.display());
                    std::process::exit(1);
                }
            };

            let interactive_asked = std::io::stdin().is_terminal();
            let controls = interactive_asked
                && std::io::stdout().is_terminal()
                && std::env::var_os("TERM").map(|t| t != "dumb").unwrap_or(true);

            #[cfg(unix)]
            if controls {
                // The loop blocks on the raw-mode reader — run it off the
                // runtime so the executor stays free.
                let handle = tokio::runtime::Handle::current();
                let tree_root = root.clone();
                let loop_workspace = workspace.clone();
                let result = tokio::task::spawn_blocking(move || {
                    run_interactive(tree_root, loop_workspace, tree, branch, handle)
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(m)) => {
                        eprintln!("amparo code: {m}");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("amparo code: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }

            print!("{}", render_report(&root, &tree, branch.as_deref()));
            #[cfg(unix)]
            if interactive_asked && !controls {
                // The operator asked for the surface but the terminal
                // cannot hold it — say why, on stderr, like the TUI does.
                eprintln!("amparo code: interactive tree needs a terminal — report follows");
            }
        }
    }
}

// ──────────────────────────────────────────────────────────── Paint ──

/// Color application for the surface. `colors: false` renders every
/// element as plain text — the piped / `NO_COLOR` contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Paint {
    colors: bool,
}

/// The palette inks, the sibling-slate family shared with the TUI.
#[derive(Clone, Copy)]
enum Ink {
    Fg,
    Dim,
    Accent,
    Ok,
    Bad,
    Warn,
    Bold,
}

impl Paint {
    /// Builds a paint with colors on or off (the test seam).
    fn with_colors(colors: bool) -> Self {
        Self { colors }
    }

    /// Detects the surface: colors only when stdout is a terminal and
    /// `NO_COLOR` is absent.
    fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self::with_colors(std::io::stdout().is_terminal() && !no_color)
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
            Ink::Bold => "1;38;2;226;232;240",
        };
        format!("\x1b[{sgr}m{text}\x1b[0m")
    }

    /// Inverse video — the selection highlight. An attribute, not a
    /// color, so it stays even under `NO_COLOR`.
    fn rev(&self, text: &str) -> String {
        format!("\x1b[7m{text}\x1b[0m")
    }
}

/// Clips `s` to `max` chars, appending `…` when cut — the TUI's
/// convention, at a column the pane chooses.
fn clip(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Clips a *painted* string to `max` display columns — SGR escapes count
/// zero columns and the paints self-close, so a cut never corrupts the
/// stream. (Clipping before the paints is what renders them wrong.)
fn clip_painted(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut seen = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            out.push(c);
            if c == 'm' {
                in_esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_esc = true;
            out.push(c);
            continue;
        }
        if seen >= max {
            continue;
        }
        seen += 1;
        out.push(c);
    }
    out
}

/// The color a git mark wears.
fn mark_text(mark: char, paint: &Paint) -> String {
    let ink = match mark {
        'M' => Ink::Warn,
        'A' => Ink::Ok,
        'D' => Ink::Bad,
        'R' => Ink::Accent,
        _ => Ink::Dim,
    };
    paint.apply(ink, &mark.to_string())
}

/// Counts display columns in a painted string — SGR escapes count zero.
#[cfg(unix)]
fn painted_columns(s: &str) -> usize {
    let mut seen = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_esc = true;
            continue;
        }
        seen += 1;
    }
    seen
}

// ─────────────────────────────────────────────────── the interactive ──

/// One node in the preorder flat array — depth, label, and the index
/// range of its subtree (files hold an empty range).
#[cfg(unix)]
struct FlatNode {
    depth: usize,
    name: String,
    is_dir: bool,
    mark: Option<char>,
    kids: std::ops::Range<usize>,
}

#[cfg(unix)]
fn flatten(tree: &Node, depth: usize, out: &mut Vec<FlatNode>) {
    let start = out.len();
    out.push(FlatNode {
        depth,
        name: tree.name.clone(),
        is_dir: tree.is_dir,
        mark: tree.mark,
        kids: 0..0,
    });
    for c in &tree.children {
        flatten(c, depth + 1, out);
    }
    out[start].kids = start + 1..out.len();
}

/// The visible rows — flat indices, with collapsed subtrees skipped.
#[cfg(unix)]
fn compute_visible(flat: &[FlatNode], collapsed: &HashSet<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < flat.len() {
        out.push(i);
        if collapsed.contains(&i) {
            i = flat[i].kids.end;
        } else {
            i += 1;
        }
    }
    out
}

/// The reading pane's state — an opened file, bounded and honest.
#[cfg(unix)]
struct ViewState {
    /// Display path for the status bar (relative to the tree root).
    rel: String,
    lines: Vec<String>,
    scroll: usize,
    truncated: bool,
}

/// Opens a file for the reading pane: capped at [`MAX_FILE_BYTES`],
/// binary files reduced to a summary row, unreadable files announced
/// rather than silently ignored.
#[cfg(unix)]
fn open_file(path: &Path, root: &Path) -> ViewState {
    use std::io::Read;
    let rel = path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    let read = (|| {
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut buf = Vec::new();
        f.by_ref()
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut buf)?;
        Ok::<_, std::io::Error>((len, buf))
    })();
    let (lines, truncated) = match read {
        Err(e) => (vec![format!("unreadable — {e}")], false),
        Ok((len, mut buf)) => {
            let truncated = buf.len() > MAX_FILE_BYTES;
            if truncated {
                buf.truncate(MAX_FILE_BYTES);
            }
            let lines = if buf[..buf.len().min(8192)].contains(&0) {
                vec![format!("binary file — {len} bytes")]
            } else {
                String::from_utf8_lossy(&buf)
                    .lines()
                    .map(str::to_string)
                    .collect()
            };
            (lines, truncated)
        }
    };
    ViewState {
        rel,
        lines,
        scroll: 0,
        truncated,
    }
}

/// One decoded key, or a tick when the 0.1s read window passed empty.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Key {
    Tick,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Quit,
    Rescan,
    /// `e` — open the edit prompt (view mode).
    Edit,
    /// `y` — apply the parked approval.
    Yes,
    /// `n` — deny the parked approval (permanent for that call).
    No,
    /// Backspace — delete the prompt's last char.
    Backspace,
    /// One printable ASCII char — prompt input.
    Char(char),
    /// A byte outside ASCII — feeds the UTF-8 accumulator.
    Byte(u8),
    Ignore,
}

/// Decodes one key from `first` (already read) plus the `next` bytes it
/// needs for its escape sequence. Pure — the unit-test seam.
#[cfg(unix)]
fn decode_key(first: u8, next: &mut dyn FnMut() -> Option<u8>) -> Key {
    match first {
        b'\r' | b'\n' => Key::Enter,
        0x03 => Key::Quit, // Ctrl-C arrives as a byte in raw mode
        b'q' | b'Q' => Key::Quit,
        b'r' | b'R' => Key::Rescan,
        b'j' | b'J' => Key::Down,
        b'k' | b'K' => Key::Up,
        b'e' | b'E' => Key::Edit,
        b'y' | b'Y' => Key::Yes,
        b'n' | b'N' => Key::No,
        0x7f | 0x08 => Key::Backspace,
        b if (0x20..=0x7e).contains(&b) => Key::Char(b as char),
        b if b >= 0x80 => Key::Byte(b),
        0x1b => {
            let Some(b2) = next() else {
                return Key::Esc; // a lone ESC, resolved by the 0.1s window
            };
            if b2 != b'[' {
                return Key::Esc; // alt-chords drop their second byte
            }
            let mut seq = Vec::new();
            let last = loop {
                match next() {
                    Some(n) if (0x40..=0x7e).contains(&n) => break n,
                    Some(_) if seq.len() > 8 => return Key::Ignore,
                    Some(n) => seq.push(n),
                    None => return Key::Esc,
                }
            };
            let params = String::from_utf8_lossy(&seq);
            match last {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'H' => Key::Home,
                b'F' => Key::End,
                b'~' => match params.as_ref() {
                    "1" | "7" => Key::Home,
                    "4" | "8" => Key::End,
                    "5" => Key::PageUp,
                    "6" => Key::PageDown,
                    _ => Key::Ignore,
                },
                _ => Key::Ignore,
            }
        }
        _ => Key::Ignore,
    }
}

#[cfg(unix)]
fn next_key(stdin: &mut std::io::Stdin) -> Key {
    // Polled, not blocking: the turn's parked approval must re-render
    // its countdown between keypresses (Key::Tick drives the frame),
    // and the Esc chord window closes on its own instead of waiting
    // for the next byte forever.
    match poll_byte(stdin, 250) {
        Some(b) => decode_key(b, &mut || poll_byte(stdin, 100)),
        None => Key::Tick,
    }
}

/// The edit prompt's input path — the TUI's prompt-byte pattern: while
/// the prompt is live, letters are letters. `r`, `q`, `e`, `y`, `n` and
/// friends stay structural everywhere else but must type here (a
/// decode-keyed `r` would rescan the tree out from under the prompt).
/// Only the bare controls are structural: Enter submits, Backspace
/// deletes, Ctrl-C cancels, and Esc (resolved through the sequence
/// window, so arrow keys arrive as their arrows and are ignored).
#[cfg(unix)]
fn next_prompt_key(stdin: &mut std::io::Stdin) -> Key {
    // Blocking is right for typed input, but the Esc chord window must
    // still close on its own (the polled `next` below).
    match read_byte(stdin) {
        Some(b) => decode_prompt_key(b, &mut || poll_byte(stdin, 100)),
        None => Key::Tick,
    }
}

/// The prompt-side decoder — [`next_prompt_key`] without the terminal,
/// for tests (the [`decode_key`] seam's shape).
#[cfg(unix)]
fn decode_prompt_key(first: u8, next: &mut dyn FnMut() -> Option<u8>) -> Key {
    match first {
        0x1b => decode_key(0x1b, next),
        b'\r' | b'\n' => Key::Enter,
        0x03 => Key::Quit,
        0x7f | 0x08 => Key::Backspace,
        b if (0x20..=0x7e).contains(&b) => Key::Char(b as char),
        b if b >= 0x80 => Key::Byte(b),
        _ => Key::Ignore,
    }
}

/// Accumulates UTF-8 input bytes: a complete sequence comes back as its
/// text (buffer cleared), an incomplete one keeps the buffer, and an
/// invalid one resets it — mojibake never reaches the prompt.
#[cfg(unix)]
fn push_utf8(buf: &mut Vec<u8>, b: u8) -> Option<String> {
    buf.push(b);
    match std::str::from_utf8(buf) {
        Ok(s) => {
            let out = s.to_string();
            buf.clear();
            Some(out)
        }
        Err(e) => {
            if e.error_len().is_some() {
                buf.clear();
            }
            None
        }
    }
}

// ─────────────────────────────────────────────────────────── edit turn ──

/// The edit turn's event log — a process-global slot (the [`Surface`]
/// callbacks are fn pointers, so they cannot capture; the TUI's `Ui`
/// slot uses the same idiom) holding the `[tag]` lines of the running
/// turn, drawn in the right pane while nothing pends approval.
#[cfg(unix)]
static EDIT_LOG: OnceLock<Arc<StdMutex<VecDeque<String>>>> = OnceLock::new();

/// Appends one line to [`EDIT_LOG`], capping it at [`EDIT_LOG_LINES`].
#[cfg(unix)]
fn push_log(line: &str) {
    let log = EDIT_LOG.get_or_init(|| Arc::new(StdMutex::new(VecDeque::new())));
    let mut log = log.lock().unwrap();
    log.push_back(line.to_string());
    while log.len() > EDIT_LOG_LINES {
        log.pop_front();
    }
}

/// The banner callback for the edit turn — the same chain facts as every
/// surface, one log line each.
#[cfg(unix)]
fn edit_banner(info: &crate::run::BannerInfo) {
    push_log(&format!("[chain] {}", info.chain));
    push_log(&format!("[infer] {}", info.infer));
    push_log(&format!("[memory] {}", info.memory));
}

/// The `[tag]` line callback for the edit turn — status lines and the
/// policy-audit notice land in the log like every other event.
#[cfg(unix)]
fn edit_line(line: &str) {
    push_log(line);
}

/// The edit turn's event sink — every loop event arrives in the log as
/// its canonical `[tag]` line.
#[cfg(unix)]
struct EditSink;

#[cfg(unix)]
impl EventSink for EditSink {
    fn emit(&self, event: &AgentEvent) {
        push_log(&format_event(event));
    }
}

/// One approval parked for the reader loop's `y`/`n` — the shared slot
/// between the gate's async side and the blocking reader.
#[cfg(unix)]
struct PendingApproval {
    request: ApprovalRequest,
    deadline: Instant,
    answer: Option<oneshot::Sender<bool>>,
}

/// The surface's approval gate (M13 W2): each request parks in the
/// shared slot and waits up to [`EDIT_APPROVAL_SECS`] for the reader
/// loop's single-key answer — timeout denies, like every other gate.
#[cfg(unix)]
struct CodeApprovalGate {
    slot: Arc<StdMutex<Option<PendingApproval>>>,
    timeout: Duration,
}

#[cfg(unix)]
impl CodeApprovalGate {
    fn new(slot: Arc<StdMutex<Option<PendingApproval>>>) -> Self {
        Self {
            slot,
            timeout: Duration::from_secs(EDIT_APPROVAL_SECS),
        }
    }

    /// The time-based test seam — a short window stands in for the 60s.
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[cfg(unix)]
#[async_trait]
impl ApprovalGate for CodeApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        let (tx, rx) = oneshot::channel();
        self.slot.lock().unwrap().replace(PendingApproval {
            request: request.clone(),
            deadline: Instant::now() + self.timeout,
            answer: Some(tx),
        });
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            // The deadline passed, or the turn was canceled mid-wait —
            // either way the call is denied. Clear the slot only when it
            // still holds *this* request, so a newer one survives.
            _ => {
                let mut guard = self.slot.lock().unwrap();
                if guard
                    .as_ref()
                    .map(|p| p.request.call_id == request.call_id)
                    .unwrap_or(false)
                {
                    guard.take();
                }
                false
            }
        }
    }
}

/// Routes one reader-loop press into the parked approval — the first
/// press wins, and a deny is permanent for that call (deny-wins).
/// Returns true when a decision was actually delivered.
#[cfg(unix)]
fn decide_pending(slot: &StdMutex<Option<PendingApproval>>, approved: bool) -> bool {
    let mut guard = slot.lock().unwrap();
    match guard.take() {
        Some(mut pending) => {
            if let Some(answer) = pending.answer.take() {
                let _ = answer.send(approved);
            }
            true
        }
        None => false,
    }
}

/// The whole seconds left on the parked approval — `None` when nothing
/// pends. Drives the status-bar countdown re-render.
#[cfg(unix)]
fn pending_remaining(slot: &StdMutex<Option<PendingApproval>>) -> Option<u64> {
    slot.lock()
        .unwrap()
        .as_ref()
        .map(|p| p.deadline.saturating_duration_since(Instant::now()).as_secs())
}

/// Reads one string argument — `(unset)` when the model omitted it.
#[cfg(unix)]
fn arg(req: &ApprovalRequest, name: &str) -> String {
    req.arguments
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("(unset)")
        .to_string()
}

/// Renders the arguments for a card header — a single-string argument
/// shows the value alone (the TUI's convention).
#[cfg(unix)]
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

/// Renders an approval request as the right-pane proposal: `edit_file`
/// as `- old` / `+ new` lines, `patch_file` as the patch verbatim, and
/// anything else as the approval card (the TUI's card copy). Pure — the
/// unit-test seam.
#[cfg(unix)]
fn approval_diff(req: &ApprovalRequest) -> Vec<String> {
    match req.tool_name.as_str() {
        "edit_file" => {
            let mut lines = vec![format!("edit_file {}", arg(req, "path"))];
            for old in arg(req, "old_text").lines() {
                lines.push(format!("- {old}"));
            }
            for new in arg(req, "new_text").lines() {
                lines.push(format!("+ {new}"));
            }
            lines
        }
        "patch_file" => {
            let mut lines = vec![format!("patch_file {}", arg(req, "path"))];
            for patch_line in arg(req, "patch").lines() {
                lines.push(patch_line.to_string());
            }
            lines
        }
        _ => {
            let why = if req.reasons.is_empty() {
                "—".to_string()
            } else {
                clip(&req.reasons.join("; "), 60)
            };
            let blast = req
                .blast_radius
                .map(|b| b.note())
                .unwrap_or("not classified");
            let rollback = match &req.rollback {
                Some(r) => clip(&r.undo, 60),
                None => "none declared".to_string(),
            };
            vec![
                format!(
                    "approval required — {} {}",
                    req.tool_name,
                    display_arg(&req.arguments)
                ),
                format!("  why:      {why}"),
                format!("  blast:    {blast}"),
                format!("  rollback: {rollback}"),
            ]
        }
    }
}

/// What the surface is doing right now — the mode drives both the key
/// routing and the right pane.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CodeMode {
    Tree,
    View,
    /// The status bar is a one-line instruction prompt (backspace-only).
    Prompt,
    /// One agent turn is running — approvals answer with `y`/`n`.
    Turn,
}

/// The surface state between frames.
#[cfg(unix)]
struct CodeState {
    flat: Vec<FlatNode>,
    collapsed: HashSet<usize>,
    visible: Vec<usize>,
    /// Selection as an index into `visible`.
    sel: usize,
    /// Tree scroll offset, in body rows.
    scroll: usize,
    /// The node budget was hit during the build — announced in the title.
    truncated: bool,
    view: Option<ViewState>,
    mode: CodeMode,
    /// The file the reading pane opened — survives `refresh`, so a
    /// rescan after an edit reopens the same file.
    view_path: Option<PathBuf>,
    /// The instruction prompt's text.
    prompt: String,
    /// One-shot status-bar notice (`(is_error, line)`) — a turn outcome
    /// or a cancelation; any next key clears it.
    notice: Option<(bool, String)>,
}

#[cfg(unix)]
impl CodeState {
    fn new(tree: &Node) -> Self {
        let mut flat = Vec::new();
        flatten(tree, 0, &mut flat);
        let visible = compute_visible(&flat, &HashSet::new());
        Self {
            flat,
            collapsed: HashSet::new(),
            visible,
            sel: 0,
            scroll: 0,
            truncated: has_truncation(tree),
            view: None,
            mode: CodeMode::Tree,
            view_path: None,
            prompt: String::new(),
            notice: None,
        }
    }

    fn refresh(&mut self, flat: Vec<FlatNode>, truncated: bool) {
        self.flat = flat;
        self.collapsed.clear();
        self.visible = compute_visible(&self.flat, &self.collapsed);
        self.sel = self.sel.min(self.visible.len().saturating_sub(1));
        self.scroll = 0;
        self.truncated = truncated;
        self.view = None;
        self.view_path = None;
        self.mode = CodeMode::Tree;
        self.prompt.clear();
        self.notice = None;
    }

    /// The flat index under the selection.
    fn selected(&self) -> usize {
        self.visible.get(self.sel).copied().unwrap_or(0)
    }

    fn move_sel(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, self.visible.len() as isize - 1) as usize;
    }

    /// Slides `scroll` so the selection sits inside the body window.
    fn follow(&mut self, body_h: usize) {
        if body_h == 0 || self.visible.is_empty() {
            return;
        }
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + body_h {
            self.scroll = self.sel + 1 - body_h;
        }
    }

    /// Applies one key; returns true when the frame changed. Any key
    /// clears the one-shot notice.
    fn handle_key(&mut self, k: Key, root: &Path, h: usize) -> bool {
        let body_h = h.saturating_sub(2).max(1);
        self.notice = None;
        match self.mode {
            CodeMode::Prompt => match k {
                Key::Enter => {
                    // The submit boundary: the loop reads this mode flip
                    // and spawns the turn (an empty instruction cancels).
                    self.mode = CodeMode::Turn;
                    true
                }
                Key::Esc | Key::Quit => {
                    self.mode = CodeMode::View;
                    self.prompt.clear();
                    true
                }
                Key::Backspace => {
                    self.prompt.pop();
                    true
                }
                Key::Char(c) => {
                    self.prompt.push(c);
                    true
                }
                _ => false,
            },
            // The turn owns the reader: only y/n/Esc are routed by the
            // loop; everything else is ignored here.
            CodeMode::Turn => false,
            CodeMode::View => match k {
                Key::Esc | Key::Quit => {
                    self.view = None;
                    self.view_path = None;
                    self.mode = CodeMode::Tree;
                    true
                }
                Key::Edit => {
                    self.mode = CodeMode::Prompt;
                    self.prompt.clear();
                    true
                }
                Key::Up | Key::Down | Key::Home | Key::End | Key::PageUp | Key::PageDown => {
                    let v = match self.view.as_mut() {
                        Some(v) => v,
                        None => return false,
                    };
                    match k {
                        Key::Up => v.scroll = v.scroll.saturating_sub(1),
                        Key::Down => {
                            v.scroll = (v.scroll + 1).min(v.lines.len().saturating_sub(1));
                        }
                        Key::Home => v.scroll = 0,
                        Key::End => v.scroll = v.lines.len().saturating_sub(body_h),
                        Key::PageUp => v.scroll = v.scroll.saturating_sub(body_h),
                        Key::PageDown => {
                            v.scroll =
                                (v.scroll + body_h).min(v.lines.len().saturating_sub(body_h));
                        }
                        _ => unreachable!(),
                    }
                    true
                }
                _ => false,
            },
            CodeMode::Tree => match k {
                Key::Up => {
                    self.move_sel(-1);
                    true
                }
                Key::Down => {
                    self.move_sel(1);
                    true
                }
                Key::Home => {
                    self.sel = 0;
                    true
                }
                Key::End => {
                    self.sel = self.visible.len().saturating_sub(1);
                    true
                }
                Key::PageUp => {
                    self.move_sel(-(body_h as isize));
                    true
                }
                Key::PageDown => {
                    self.move_sel(body_h as isize);
                    true
                }
                Key::Enter => {
                    let idx = self.selected();
                    let (is_dir, name) = {
                        let f = &self.flat[idx];
                        (f.is_dir, f.name.clone())
                    };
                    if is_dir {
                        if !self.collapsed.remove(&idx) {
                            self.collapsed.insert(idx);
                        }
                        self.visible = compute_visible(&self.flat, &self.collapsed);
                        self.sel = self.sel.min(self.visible.len().saturating_sub(1));
                    } else {
                        // The path rebuilds from the flat order: every
                        // ancestor dir's subtree range contains `idx`.
                        // The root node itself (always index 0) is skipped —
                        // `root` already carries its full path.
                        let mut path = root.to_path_buf();
                        for f in self.flat.iter().take(idx).skip(1) {
                            if f.is_dir && f.kids.contains(&idx) {
                                path.push(&f.name);
                            }
                        }
                        path.push(&name);
                        self.view = Some(open_file(&path, root));
                        self.view_path = Some(path);
                        self.mode = CodeMode::View;
                    }
                    true
                }
                _ => false,
            },
        }
    }
}

/// Renders one frame. Every row is clipped to its pane and erased to the
/// end of the line, so a resize leaves no stale cells.
#[cfg(unix)]
fn draw(
    state: &CodeState,
    root: &Path,
    branch: Option<&str>,
    paint: &Paint,
    w: usize,
    h: usize,
    pending: Option<(&ApprovalRequest, Instant)>,
    log: &VecDeque<String>,
) -> String {
    let mut out = String::new();
    out.push_str("\x1b[?25l\x1b[H");

    // Title row.
    let brand = paint.apply(Ink::Bold, "amparo code");
    let git = branch
        .map(|b| format!(" · {b}"))
        .unwrap_or_else(|| " · not a git repository".to_string());
    let trunc = if state.truncated { " · truncated" } else { "" };
    let title = format!("{brand} — {}{git}{trunc}", root.display());
    out.push_str(&clip_painted(&paint.apply(Ink::Dim, &title), w));
    out.push_str("\x1b[K\r\n");

    let body_h = h.saturating_sub(2);
    let tree_w = if w >= 70 { (w / 2).min(48) } else { 28.min(w) };
    let has_right = w >= tree_w + 22;
    let pane_w = w.saturating_sub(tree_w + 1);

    // The turn's right pane: the pending diff (with its deadline), or
    // the event-log tail while the agent works.
    let turn_lines: Vec<String> = if state.mode == CodeMode::Turn {
        match pending {
            Some((req, deadline)) => {
                let mut lines = approval_diff(req);
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                lines.push(format!("── {remaining}s left — deny on timeout"));
                lines
            }
            None => log
                .iter()
                .skip(log.len().saturating_sub(body_h))
                .cloned()
                .collect(),
        }
    } else {
        Vec::new()
    };

    for r in 0..body_h {
        // Left: the tree.
        let row_idx = state.scroll + r;
        let mut line = String::new();
        if row_idx < state.visible.len() {
            let f = &state.flat[state.visible[row_idx]];
            let indent = "  ".repeat(f.depth.min(12));
            let glyph = if f.is_dir { "▸ " } else { "  " };
            let mark_cols = if f.mark.is_some() { 2 } else { 0 }; // space + mark
            let name_budget = tree_w
                .saturating_sub(indent.chars().count() + 2 + mark_cols)
                .max(1);
            let name = clip(&f.name, name_budget);
            // Paint the composed row: indentation and glyph dim, the name
            // colored, the mark by state — then clip by display columns.
            let painted = paint.apply(Ink::Dim, &indent)
                + &paint.apply(Ink::Dim, glyph)
                + &paint.apply(if f.is_dir { Ink::Accent } else { Ink::Fg }, &name)
                + &f.mark.map(|m| format!(" {}", mark_text(m, paint))).unwrap_or_default();
            let mut row = clip_painted(&painted, tree_w);
            if row_idx == state.sel {
                row = paint.rev(&row);
            }
            line.push_str(&row);
        }
        line.push_str("\x1b[K");
        if has_right {
            line.push_str(&paint.apply(Ink::Dim, "│"));
            // Right: the mode's pane — the turn's diff or log, the
            // prompt's hints, the opened file, or the tree hints.
            let right = match state.mode {
                CodeMode::Turn => turn_lines.get(r).map_or(String::new(), |line| {
                    let ink = if line.starts_with('-') {
                        Ink::Bad
                    } else if line.starts_with('+') {
                        Ink::Ok
                    } else if r == 0 {
                        Ink::Bold
                    } else {
                        Ink::Dim
                    };
                    paint.apply(ink, &clip(line, pane_w))
                }),
                CodeMode::Prompt => match r {
                    0 => paint.apply(Ink::Dim, "editing — one instruction, ↵ submits"),
                    1 => paint.apply(
                        Ink::Dim,
                        "the turn runs behind the same gate chain as `amparo run`",
                    ),
                    _ => String::new(),
                },
                CodeMode::View => match &state.view {
                    Some(v) => {
                        let idx = v.scroll + r;
                        match v.lines.get(idx) {
                            Some(content) => {
                                let num = paint.apply(Ink::Dim, &format!("{:>4} ", idx + 1));
                                format!("{num}{}", clip(content, pane_w.saturating_sub(5)))
                            }
                            None => String::new(),
                        }
                    }
                    None => String::new(),
                },
                CodeMode::Tree => match r {
                    0 => paint.apply(Ink::Dim, "↵ open a file — it opens here"),
                    1 => {
                        let f = &state.flat[state.selected()];
                        paint.apply(
                            Ink::Dim,
                            if f.is_dir {
                                "↵ toggles a directory"
                            } else {
                                "↵ opens the selected file — then e edits it"
                            },
                        )
                    }
                    2 => paint.apply(Ink::Dim, "↑↓ move · r rescan · q quit"),
                    _ => String::new(),
                },
            };
            line.push_str(&clip_painted(&right, pane_w));
            line.push_str("\x1b[K");
        }
        out.push_str(&line);
        out.push_str("\r\n");
    }

    // Status bar, inverse across the full width. A one-shot notice
    // replaces the legend entirely.
    let legend = match state.mode {
        CodeMode::Prompt => {
            let rel = state
                .view_path
                .as_ref()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .unwrap_or_else(|| "file".to_string());
            format!(
                "edit {rel}: {}█ · ↵ submit · esc cancel",
                clip(&state.prompt, w.saturating_sub(30).max(8))
            )
        }
        CodeMode::Turn => match pending {
            Some((_, deadline)) => {
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                format!("y apply · n deny ({remaining}s)")
            }
            None => "edit running — y/n answer approvals · esc cancel".to_string(),
        },
        CodeMode::View => {
            let trunc = state
                .view
                .as_ref()
                .map(|v| if v.truncated { " · truncated" } else { "" })
                .unwrap_or("");
            let (rel, line, total) = state
                .view
                .as_ref()
                .map(|v| (v.rel.as_str(), v.scroll + 1, v.lines.len().max(1)))
                .unwrap_or(("", 1, 1));
            format!("viewing {rel} — line {line}/{total} · ↑↓ scroll · e edit · esc back · q quit{trunc}")
        }
        CodeMode::Tree => "↑↓ move · ↵ open · r rescan · q quit".to_string(),
    };
    let legend = match &state.notice {
        Some((is_error, line)) => paint.apply(
            if *is_error { Ink::Bad } else { Ink::Warn },
            &clip(line, w.saturating_sub(20).max(10)),
        ),
        None => legend,
    };
    let left = clip(
        &format!("amparo code — {}", root.display()),
        w.saturating_sub(legend.chars().count() + 3).max(8),
    );
    let status = format!(" {left} · {legend}");
    let bar = format!(
        "{status}{}",
        " ".repeat(w.saturating_sub(painted_columns(&status)))
    );
    out.push_str(&paint.rev(&clip_painted(&bar, w)));
    out.push('\n');
    out
}

/// The interactive loop: alternate screen, raw mode, keys until `q`.
#[cfg(unix)]
fn run_interactive(
    root: PathBuf,
    workspace: PathBuf,
    tree: Node,
    branch: Option<String>,
    handle: tokio::runtime::Handle,
) -> Result<(), String> {
    use std::io::Write;

    let mut stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let paint = Paint::detect();
    let mut branch = branch;

    // The edit machinery: the process-global event log (the Surface
    // callbacks are fn pointers) and the shared approval slot the gate
    // parks requests in for the reader loop's y/n. A fresh surface
    // starts with a fresh log.
    let pending: Arc<StdMutex<Option<PendingApproval>>> = Arc::new(StdMutex::new(None));
    let log = EDIT_LOG.get_or_init(|| Arc::new(StdMutex::new(VecDeque::new())));
    log.lock().unwrap().clear();

    // Alternate screen first, then raw mode — a crash mid-entry leaves
    // the terminal in its original screen with cooked input.
    out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")
        .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    // Raw mode is best-effort: on a terminal that refuses it, keys stay
    // line-buffered and the surface still renders (the TUI's convention).
    let _guard = enter_raw_mode(); // restores termios on drop, always

    let mut state = CodeState::new(&tree);
    let mut dirty = true;
    let mut last_size = term_size().unwrap_or((80, 24));
    let mut edit_task: Option<tokio::task::JoinHandle<Result<String, String>>> = None;
    let mut last_remaining: Option<u64> = None;
    let mut utf8_buf: Vec<u8> = Vec::new();

    let result = (|| -> Result<(), String> {
        loop {
            if let Some(size) = term_size() {
                if size != last_size {
                    last_size = size;
                    dirty = true;
                }
            }
            // The edit turn finished (or died) since the last frame —
            // fold its outcome back into the surface.
            if edit_task.as_ref().map(|t| t.is_finished()).unwrap_or(false) {
                let task = edit_task.take().expect("checked");
                let outcome = match handle.block_on(task) {
                    Ok(r) => r,
                    Err(_) => Ok("edit canceled".to_string()),
                };
                // A parked approval the turn never resolved dies with
                // the turn — clear the slot.
                pending.lock().unwrap().take();
                let reopen = state.view_path.clone();
                if reopen.is_some() {
                    // Fresh marks + a fresh tree: the edit may have
                    // changed the workspace (git status included).
                    let (marks, fresh) = handle.block_on(git_marks(&workspace));
                    if fresh.is_some() {
                        branch = fresh;
                    }
                    let mut budget = MAX_TREE_NODES;
                    if let Ok(t) = build_tree(&root, &marks, &mut budget) {
                        let mut flat = Vec::new();
                        flatten(&t, 0, &mut flat);
                        state.refresh(flat, has_truncation(&t));
                    }
                    if let Some(path) = reopen {
                        state.view = Some(open_file(&path, &root));
                        state.view_path = Some(path);
                    }
                    state.mode = CodeMode::View;
                }
                // The outcome always speaks — success and failure alike.
                state.notice = Some((outcome.is_err(), outcome.unwrap_or_else(|e| e)));
                last_remaining = None;
                dirty = true;
            }
            if dirty {
                let (w, h) = last_size;
                let frame = {
                    let pending_guard = pending.lock().unwrap();
                    let pending_view = pending_guard
                        .as_ref()
                        .map(|p| (&p.request, p.deadline));
                    let log_guard = log.lock().unwrap();
                    draw(
                        &state,
                        &root,
                        branch.as_deref(),
                        &paint,
                        w,
                        h,
                        pending_view,
                        &log_guard,
                    )
                };
                out.write_all(frame.as_bytes()).map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                dirty = false;
            }
            // The prompt takes its input as raw bytes (letters stay
            // letters); every other mode decodes structural keys.
            let key = if state.mode == CodeMode::Prompt {
                next_prompt_key(&mut stdin)
            } else {
                next_key(&mut stdin)
            };
            match key {
                Key::Tick => {
                    // The countdown may have ticked — re-render when the
                    // remaining seconds changed.
                    if state.mode == CodeMode::Turn {
                        let remaining = pending_remaining(&pending);
                        if last_remaining != remaining {
                            last_remaining = remaining;
                            dirty = true;
                        }
                    }
                }
                Key::Quit => {
                    if state.mode == CodeMode::Turn {
                        // Esc/q cancels the whole turn: deny whatever
                        // pends, stop the task, back to the file.
                        decide_pending(&pending, false);
                        if let Some(task) = edit_task.take() {
                            task.abort();
                        }
                        state.mode = CodeMode::View;
                        state.notice = Some((false, "edit canceled".to_string()));
                        last_remaining = None;
                        dirty = true;
                    } else if state.mode == CodeMode::Tree {
                        break; // q quits from the tree, backs out elsewhere
                    } else if state.handle_key(key, &root, last_size.1) {
                        state.follow(last_size.1.saturating_sub(2).max(1));
                        dirty = true;
                    }
                }
                Key::Esc => {
                    if state.mode == CodeMode::Turn {
                        decide_pending(&pending, false);
                        if let Some(task) = edit_task.take() {
                            task.abort();
                        }
                        state.mode = CodeMode::View;
                        state.notice = Some((false, "edit canceled".to_string()));
                        last_remaining = None;
                        dirty = true;
                    } else if state.handle_key(key, &root, last_size.1) {
                        state.follow(last_size.1.saturating_sub(2).max(1));
                        dirty = true;
                    }
                }
                Key::Yes | Key::No => {
                    if state.mode == CodeMode::Turn {
                        decide_pending(&pending, key == Key::Yes);
                        last_remaining = None;
                        dirty = true;
                    } else if state.handle_key(key, &root, last_size.1) {
                        // y/n typed into the prompt are plain characters.
                        state.follow(last_size.1.saturating_sub(2).max(1));
                        dirty = true;
                    }
                }
                Key::Byte(b) => {
                    // Multi-byte input accumulates here; complete
                    // sequences feed the prompt as their characters.
                    if state.mode == CodeMode::Prompt {
                        if let Some(s) = push_utf8(&mut utf8_buf, b) {
                            for c in s.chars() {
                                state.handle_key(Key::Char(c), &root, last_size.1);
                            }
                            dirty = true;
                        }
                    }
                }
                Key::Rescan => {
                    if state.mode == CodeMode::Turn {
                        // The turn owns the reader — nothing else routes.
                    } else {
                        // Fresh marks + a fresh tree — the workspace
                        // moved under us. Blocking the reader thread on
                        // the handle is fine: this is the only
                        // interactive task.
                        let (marks, fresh) = handle.block_on(git_marks(&workspace));
                        if fresh.is_some() {
                            branch = fresh;
                        }
                        let mut budget = MAX_TREE_NODES;
                        if let Ok(t) = build_tree(&root, &marks, &mut budget) {
                            let mut flat = Vec::new();
                            flatten(&t, 0, &mut flat);
                            state.refresh(flat, has_truncation(&t));
                        }
                        dirty = true;
                    }
                }
                k => {
                    if state.mode != CodeMode::Turn
                        && state.handle_key(k, &root, last_size.1)
                    {
                        state.follow(last_size.1.saturating_sub(2).max(1));
                        dirty = true;
                    }
                }
            }
            // A mode change out of the prompt drops its partial UTF-8
            // sequence, so stale bytes never leak into the next prompt.
            if state.mode != CodeMode::Prompt {
                utf8_buf.clear();
            }
            // Prompt → Turn is the submit boundary: spawn the turn, or
            // treat an empty instruction as a cancel.
            if state.mode == CodeMode::Turn && edit_task.is_none() {
                let instruction = std::mem::take(&mut state.prompt);
                if instruction.trim().is_empty() {
                    state.mode = CodeMode::View;
                } else {
                    let rel = state
                        .view_path
                        .as_ref()
                        .map(|p| {
                            p.strip_prefix(&root)
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|_| p.display().to_string())
                        })
                        .unwrap_or_else(|| "(file unknown)".to_string());
                    let prompt = format!("{instruction} (file: {rel})");
                    edit_task = Some(handle.spawn(run_edit_turn(
                        prompt,
                        run::new_task_id(),
                        Arc::clone(&pending),
                    )));
                }
                last_remaining = None;
                dirty = true;
            }
        }
        Ok(())
    })();

    // Leave clean: cursor back, colors off, alternate screen off, then
    // the termios guard restores cooked input as it drops.
    out.write_all(b"\x1b[?25h\x1b[0m\x1b[?1049l")
        .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    result
}

/// One agent turn for the edit prompt: the same gate chain as
/// `amparo run` — the profile, fail-closed inference, the policy engine,
/// and this surface's approval gate — its events drawn into the log and
/// the right pane. Returns the outcome line for the status bar.
#[cfg(unix)]
async fn run_edit_turn(
    prompt: String,
    task_id: String,
    pending: Arc<StdMutex<Option<PendingApproval>>>,
) -> Result<String, String> {
    // The diff-accept contract (M13 W2): every edit asks its y/n question,
    // so edits park at the gate here. No CLI flag — other surfaces keep
    // the default (ExternalEffector and a policy Escalate still park).
    let mut flags = RunFlags::default();
    flags.approval_threshold = ToolTrustTier::LocalMutating;
    let gate: Arc<dyn ApprovalGate> = Arc::new(CodeApprovalGate::new(Arc::clone(&pending)));
    let surface = Surface {
        events: Arc::new(EditSink),
        approval: Some((
            gate,
            "you, at this terminal — 60s fail-closed".to_string(),
        )),
        banner: edit_banner,
        line: edit_line,
        notice: edit_line,
    };
    let mut wired = run::wire_with(&flags, task_id, surface).await?;
    let fires = wired.schedule_fires.drain(..).collect::<Vec<_>>();
    let report = wired.agent.run(prompt).await;
    for fire in fires {
        let _ = fire.await;
    }
    match report.status {
        TaskStatus::Complete => Ok(format!("[report] complete — {} step(s)", report.steps_used)),
        TaskStatus::Failed => Err(format!(
            "edit failed — {}",
            report.final_answer.unwrap_or_else(|| "no final answer".to_string())
        )),
    }
}

// ───────────────────────────────────────────────────────────── tests ──

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test scratch directory — unique to the test, never shared
    /// (parallel test processes would fight over one).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amparo-code-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
        std::fs::write(path, "x").expect("write file");
    }

    #[test]
    fn parse_flags_shape() {
        assert!(matches!(
            parse_code_flags(["--help".to_string()].into_iter()),
            ParseCodeResult::Help
        ));
        assert!(matches!(
            parse_code_flags(["a".to_string(), "b".to_string()].into_iter()),
            ParseCodeResult::Error(_)
        ));
        assert!(matches!(
            parse_code_flags(["--bogus".to_string()].into_iter()),
            ParseCodeResult::Error(_)
        ));
        match parse_code_flags(["dir".to_string()].into_iter()) {
            ParseCodeResult::Run(d) => assert_eq!(d, PathBuf::from("dir")),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn tree_sorts_dirs_first_and_skips_ignored() {
        let root = scratch("sort");
        touch(&root, "b.txt");
        touch(&root, "a.txt");
        touch(&root, "dir_z/nested.txt");
        touch(&root, "dir_a/nested.txt");
        touch(&root, ".git/config");
        touch(&root, "target/build");
        touch(&root, "node_modules/pkg/index.js");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let names: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dir_a", "dir_z", "a.txt", "b.txt"]);
        let (files, dirs) = count(&tree);
        // 4 files: a.txt, b.txt, dir_a/nested.txt, dir_z/nested.txt.
        // 3 dirs: the root, dir_a, dir_z. The ignored dirs contribute none.
        assert_eq!((files, dirs), (4, 3));
        assert!(!has_truncation(&tree));
    }

    #[test]
    fn tree_budget_truncates_honestly() {
        let root = scratch("budget");
        touch(&root, "a.txt");
        touch(&root, "b.txt");
        touch(&root, "c.txt");
        let mut budget = 3; // root + two files
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        assert!(has_truncation(&tree));
        assert!(tree.children.len() <= 2);
        assert_eq!(budget, 0);
    }

    #[test]
    fn tree_applies_marks() {
        let root = scratch("marks");
        touch(&root, "clean.rs");
        touch(&root, "dirty.rs");
        let mut marks = HashMap::new();
        marks.insert(root.join("dirty.rs"), 'M');
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &marks, &mut budget).expect("build");
        let dirty = tree.children.iter().find(|c| c.name == "dirty.rs").expect("dirty");
        assert_eq!(dirty.mark, Some('M'));
        let clean = tree.children.iter().find(|c| c.name == "clean.rs").expect("clean");
        assert_eq!(clean.mark, None);
    }

    #[test]
    fn porcelain_lines_map_marks_and_paths() {
        assert_eq!(porcelain_line("## main...origin/main"), None);
        assert_eq!(
            porcelain_line(" M src/code.rs"),
            Some(('M', "src/code.rs".to_string()))
        );
        assert_eq!(
            porcelain_line("?? new file.txt"),
            Some(('?', "new file.txt".to_string()))
        );
        assert_eq!(
            porcelain_line("R  old.rs -> new.rs"),
            Some(('R', "new.rs".to_string()))
        );
        assert_eq!(
            porcelain_line("?? \"sp ace.rs\""),
            Some(('?', "sp ace.rs".to_string()))
        );
        assert_eq!(
            porcelain_line("?? \"we\\\"ird.rs\""),
            Some(('?', "we\"ird.rs".to_string()))
        );
        assert_eq!(porcelain_line("XY nope"), None);
    }

    #[test]
    fn report_is_plain_and_deterministic() {
        let root = scratch("report");
        touch(&root, "src/main.rs");
        touch(&root, "src/lib.rs");
        touch(&root, "Cargo.toml");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let report = render_report(&root, &tree, None);
        assert!(!report.contains("\x1b["), "zero escapes: {report}");
        assert!(report.contains("amparo code —"), "{report}");
        assert!(report.contains("not a git repository"), "{report}");
        assert!(report.contains("src/"), "{report}");
        assert!(report.contains("main.rs"), "{report}");
        assert_eq!(report, render_report(&root, &tree, None), "deterministic");
        let with_branch = render_report(&root, &tree, Some("main"));
        assert!(with_branch.contains("branch main"), "{with_branch}");
    }

    #[test]
    fn report_notes_truncation() {
        let root = scratch("report-trunc");
        touch(&root, "a.txt");
        touch(&root, "b.txt");
        let mut budget = 2;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let report = render_report(&root, &tree, None);
        assert!(report.contains("truncated"), "{report}");
    }

    #[test]
    fn clip_painted_counts_columns_not_escapes() {
        let paint = Paint::with_colors(true);
        let painted = paint.apply(Ink::Dim, "abcdef");
        assert_eq!(clip_painted(&painted, 3), format!("{}abc\x1b[0m", "\x1b[2;38;2;148;163;184m"));
        assert_eq!(clip_painted("abc", 5), "abc");
    }

    #[cfg(unix)]
    fn feed(bytes: &[u8]) -> impl FnMut() -> Option<u8> + '_ {
        let mut it = bytes.iter().cloned();
        move || it.next()
    }

    #[cfg(unix)]
    #[test]
    fn key_decoder_routes_sequences() {
        assert_eq!(decode_key(b'q', &mut feed(b"")), Key::Quit);
        assert_eq!(decode_key(b'\r', &mut feed(b"")), Key::Enter);
        assert_eq!(decode_key(0x03, &mut feed(b"")), Key::Quit);
        assert_eq!(decode_key(b'j', &mut feed(b"")), Key::Down);
        assert_eq!(decode_key(b'k', &mut feed(b"")), Key::Up);
        assert_eq!(decode_key(0x1b, &mut feed(b"[A")), Key::Up);
        assert_eq!(decode_key(0x1b, &mut feed(b"[B")), Key::Down);
        assert_eq!(decode_key(0x1b, &mut feed(b"[5~")), Key::PageUp);
        assert_eq!(decode_key(0x1b, &mut feed(b"[6~")), Key::PageDown);
        assert_eq!(decode_key(0x1b, &mut feed(b"[H")), Key::Home);
        assert_eq!(decode_key(0x1b, &mut feed(b"[F")), Key::End);
        assert_eq!(decode_key(0x1b, &mut feed(b"[1~")), Key::Home);
        assert_eq!(decode_key(0x1b, &mut feed(b"[4~")), Key::End);
        assert_eq!(decode_key(0x1b, &mut feed(b"[x")), Key::Ignore);
        assert_eq!(decode_key(0x1b, &mut feed(b"")), Key::Esc);
        assert_eq!(decode_key(0x1b, &mut feed(b"x")), Key::Esc);
    }

    #[cfg(unix)]
    #[test]
    fn collapse_skips_subtrees() {
        let root = scratch("collapse");
        touch(&root, "dir_a/x.txt");
        touch(&root, "dir_a/sub/y.txt");
        touch(&root, "z.txt");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        let all = compute_visible(&flat, &HashSet::new());
        assert_eq!(all.len(), 6); // root, dir_a, x, sub, y, z
        let dir_a = flat.iter().position(|f| f.name == "dir_a").expect("dir_a");
        let mut collapsed = HashSet::new();
        collapsed.insert(dir_a);
        let visible = compute_visible(&flat, &collapsed);
        assert!(visible.contains(&dir_a));
        assert!(!visible.contains(&flat.iter().position(|f| f.name == "x.txt").unwrap()));
        assert!(!visible.contains(&flat.iter().position(|f| f.name == "y.txt").unwrap()));
        assert!(visible.contains(&flat.iter().position(|f| f.name == "z.txt").unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn enter_on_a_file_opens_the_reading_pane_with_content() {
        // The regression this guards: the ancestor walk once pushed the
        // root node's own name onto the path, so every open failed with a
        // doubled directory component.
        let root = scratch("open");
        touch(&root, "src/main.rs");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("content");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        let visible = compute_visible(&flat, &HashSet::new());
        let mut state = CodeState {
            flat,
            collapsed: HashSet::new(),
            visible,
            sel: 0,
            scroll: 0,
            truncated: false,
            view: None,
            mode: CodeMode::Tree,
            view_path: None,
            prompt: String::new(),
            notice: None,
        };
        // Move the selection to main.rs (root, src, main.rs in preorder).
        state.sel = 2;
        assert!(state.handle_key(Key::Enter, &root, 24));
        let view = state.view.expect("view opened");
        assert_eq!(view.rel, "src/main.rs");
        assert_eq!(view.lines, vec!["fn main() {}"]);
        assert_eq!(state.mode, CodeMode::View);
        assert_eq!(state.view_path, Some(root.join("src/main.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn frame_rows_match_terminal_height() {
        let root = scratch("frame");
        touch(&root, "a.txt");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let state = CodeState::new(&tree);
        let paint = Paint::with_colors(false);
        let frame = draw(&state, &root, None, &paint, 80, 24, None, &VecDeque::new());
        assert_eq!(frame.lines().count(), 24, "{frame}");
        assert!(frame.starts_with("\x1b[?25l\x1b[H"), "{frame}");
    }

    #[cfg(unix)]
    #[test]
    fn selection_follows_into_view() {
        let mut state = CodeState::new(&Node {
            name: "root".into(),
            is_dir: true,
            mark: None,
            children: Vec::new(),
            truncated: false,
        });
        state.flat = vec![
            FlatNode {
                depth: 0,
                name: "root".into(),
                is_dir: true,
                mark: None,
                kids: 1..4,
            },
            FlatNode { depth: 1, name: "a".into(), is_dir: false, mark: None, kids: 0..0 },
            FlatNode { depth: 1, name: "b".into(), is_dir: false, mark: None, kids: 0..0 },
            FlatNode { depth: 1, name: "c".into(), is_dir: false, mark: None, kids: 0..0 },
        ];
        state.visible = compute_visible(&state.flat, &state.collapsed);
        state.sel = 3;
        state.follow(2); // body height 2 — the last row must scroll into view
        assert_eq!(state.scroll, 2);
    }

    /// A minimal edit_file approval request — the shape the drill's mock
    /// model produces.
    #[cfg(unix)]
    fn edit_request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "call-1".to_string(),
            tool_name: "edit_file".to_string(),
            arguments: serde_json::json!({
                "path": "note.txt",
                "old_text": "hello\n",
                "new_text": "hello from the drill\n"
            }),
            reasons: vec!["policy allows this write".to_string()],
            blast_radius: None,
            session_label: None,
            rollback: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn key_decoder_routes_edit_keys() {
        assert_eq!(decode_key(b'e', &mut feed(b"")), Key::Edit);
        assert_eq!(decode_key(b'E', &mut feed(b"")), Key::Edit);
        assert_eq!(decode_key(b'y', &mut feed(b"")), Key::Yes);
        assert_eq!(decode_key(b'n', &mut feed(b"")), Key::No);
        assert_eq!(decode_key(0x7f, &mut feed(b"")), Key::Backspace);
        assert_eq!(decode_key(0x08, &mut feed(b"")), Key::Backspace);
        assert_eq!(decode_key(b' ', &mut feed(b"")), Key::Char(' '));
        assert_eq!(decode_key(0xC3, &mut feed(b"")), Key::Byte(0xC3));
    }

    #[cfg(unix)]
    #[test]
    fn prompt_key_keeps_letters_as_letters() {
        // The prompt must accept every letter — `r`/`q`/`e`/`y`/`n` are
        // structural outside it, but they type here (a decode-keyed `r`
        // would rescan the tree out from under the prompt).
        assert_eq!(decode_prompt_key(b'r', &mut feed(b"")), Key::Char('r'));
        assert_eq!(decode_prompt_key(b'q', &mut feed(b"")), Key::Char('q'));
        assert_eq!(decode_prompt_key(b'e', &mut feed(b"")), Key::Char('e'));
        assert_eq!(decode_prompt_key(b'y', &mut feed(b"")), Key::Char('y'));
        assert_eq!(decode_prompt_key(b'n', &mut feed(b"")), Key::Char('n'));
        assert_eq!(decode_prompt_key(b'\r', &mut feed(b"")), Key::Enter);
        assert_eq!(decode_prompt_key(b'\n', &mut feed(b"")), Key::Enter);
        assert_eq!(decode_prompt_key(0x03, &mut feed(b"")), Key::Quit);
        assert_eq!(decode_prompt_key(0x7f, &mut feed(b"")), Key::Backspace);
        assert_eq!(decode_prompt_key(0xC3, &mut feed(b"")), Key::Byte(0xC3));
        assert_eq!(decode_prompt_key(b'\t', &mut feed(b"")), Key::Ignore);
        assert_eq!(decode_prompt_key(0x1b, &mut feed(b"[A")), Key::Up);
    }

    #[cfg(unix)]
    #[test]
    fn push_utf8_accumulates_and_resets() {
        let mut buf = Vec::new();
        assert_eq!(push_utf8(&mut buf, b'a'), Some("a".to_string()));
        assert_eq!(push_utf8(&mut buf, 0xC3), None); // first byte of é
        assert_eq!(push_utf8(&mut buf, 0xA9), Some("é".to_string()));
        assert_eq!(push_utf8(&mut buf, 0xFF), None); // invalid lead byte — dropped
        assert_eq!(push_utf8(&mut buf, b'x'), Some("x".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn approval_diff_renders_edit_file_as_a_diff() {
        let lines = approval_diff(&edit_request());
        assert_eq!(lines[0], "edit_file note.txt");
        assert!(lines.contains(&"- hello".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"+ hello from the drill".to_string()),
            "{lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn approval_diff_renders_patch_file_verbatim() {
        let mut req = edit_request();
        req.tool_name = "patch_file".to_string();
        req.arguments = serde_json::json!({
            "path": "note.txt",
            "patch": "@@ -1 +1 @@\n-hello\n+hello from the drill\n"
        });
        let lines = approval_diff(&req);
        assert_eq!(lines[0], "patch_file note.txt");
        assert!(lines.contains(&"@@ -1 +1 @@".to_string()), "{lines:?}");
        assert!(lines.contains(&"-hello".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"+hello from the drill".to_string()),
            "{lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn approval_diff_renders_a_card_for_other_tools() {
        let mut req = edit_request();
        req.tool_name = "shell_command".to_string();
        req.arguments = serde_json::json!({"command": "rm -rf /"});
        let lines = approval_diff(&req);
        assert_eq!(lines[0], "approval required — shell_command rm -rf /");
        assert!(lines[1].starts_with("  why:      policy allows"), "{lines:?}");
        assert_eq!(lines[2], "  blast:    not classified");
        assert_eq!(lines[3], "  rollback: none declared");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn code_approval_gate_answers_a_single_key_and_times_out() {
        let slot: Arc<StdMutex<Option<PendingApproval>>> = Arc::new(StdMutex::new(None));
        let gate = Arc::new(
            CodeApprovalGate::new(Arc::clone(&slot)).with_timeout(Duration::from_millis(200)),
        );

        // A y press while the request parks resolves the gate with true.
        let req = edit_request();
        let task = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.request(&req).await })
        };
        for _ in 0..100 {
            if slot.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(decide_pending(&slot, true), "request must be parked");
        assert!(task.await.expect("task"), "y resolves true");

        // No press: the deadline expires and the call is denied, and the
        // slot is cleared so the next request starts clean.
        let req = edit_request();
        let task = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.request(&req).await })
        };
        for _ in 0..100 {
            if slot.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!task.await.expect("task"), "timeout denies");
        assert!(slot.lock().unwrap().is_none(), "slot cleared after the timeout");
    }

    #[cfg(unix)]
    #[test]
    fn decide_pending_first_press_wins() {
        let (tx, mut rx) = oneshot::channel();
        let slot = StdMutex::new(Some(PendingApproval {
            request: edit_request(),
            deadline: Instant::now() + Duration::from_secs(60),
            answer: Some(tx),
        }));
        assert!(decide_pending(&slot, true));
        assert!(
            !decide_pending(&slot, false),
            "the slot is empty after the first press"
        );
        assert_eq!(rx.try_recv(), Ok(true));
    }

    #[cfg(unix)]
    #[test]
    fn edit_prompt_input_chars_backspace_esc_enter() {
        let root = scratch("prompt");
        touch(&root, "a.txt");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let mut state = CodeState::new(&tree);
        state.sel = 1; // a.txt (root, a.txt in preorder)
        assert!(state.handle_key(Key::Enter, &root, 24));
        assert_eq!(state.mode, CodeMode::View);

        // e opens the prompt; letters accumulate; backspace deletes.
        assert!(state.handle_key(Key::Edit, &root, 24));
        assert_eq!(state.mode, CodeMode::Prompt);
        assert!(state.handle_key(Key::Char('a'), &root, 24));
        assert!(state.handle_key(Key::Char('b'), &root, 24));
        assert!(state.handle_key(Key::Backspace, &root, 24));
        assert_eq!(state.prompt, "a");

        // Esc cancels back to the file, prompt cleared.
        assert!(state.handle_key(Key::Esc, &root, 24));
        assert_eq!(state.mode, CodeMode::View);
        assert_eq!(state.prompt, "");

        // Enter submits: the mode flips to Turn, the text kept for the
        // loop's submit boundary.
        assert!(state.handle_key(Key::Edit, &root, 24));
        assert!(state.handle_key(Key::Char('x'), &root, 24));
        assert!(state.handle_key(Key::Enter, &root, 24));
        assert_eq!(state.mode, CodeMode::Turn);
        assert_eq!(state.prompt, "x");
    }

    #[cfg(unix)]
    #[test]
    fn pending_edit_renders_the_diff_in_the_right_pane() {
        let root = scratch("draw-diff");
        touch(&root, "a.txt");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let mut state = CodeState::new(&tree);
        state.mode = CodeMode::Turn;
        let req = edit_request();
        let deadline = Instant::now() + Duration::from_secs(30);
        let paint = Paint::with_colors(false);
        let frame = draw(
            &state,
            &root,
            None,
            &paint,
            90,
            24,
            Some((&req, deadline)),
            &VecDeque::new(),
        );
        assert!(frame.contains("edit_file note.txt"), "{frame}");
        assert!(frame.contains("- hello"), "{frame}");
        assert!(frame.contains("+ hello from the drill"), "{frame}");
        assert!(frame.contains("y apply · n deny ("), "{frame}");
        assert!(frame.contains("left — deny on timeout"), "{frame}");
    }

    #[cfg(unix)]
    #[test]
    fn turn_pane_shows_the_event_log_when_nothing_pends() {
        let root = scratch("draw-log");
        touch(&root, "a.txt");
        let mut budget = MAX_TREE_NODES;
        let tree = build_tree(&root, &HashMap::new(), &mut budget).expect("build");
        let mut state = CodeState::new(&tree);
        state.mode = CodeMode::Turn;
        let mut log = VecDeque::new();
        log.push_back("[chain] policy → allow".to_string());
        log.push_back("[infer] mock provider".to_string());
        log.push_back("[report] complete — 2 step(s)".to_string());
        let paint = Paint::with_colors(false);
        let frame = draw(&state, &root, None, &paint, 90, 24, None, &log);
        assert!(frame.contains("[chain] policy"), "{frame}");
        assert!(frame.contains("[report] complete"), "{frame}");
    }
}
