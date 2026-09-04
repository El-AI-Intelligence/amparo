//! `amparo code` — the coding-terminal surface (M13).
//!
//! An alternate-screen file tree, greenfield hand-rolled ANSI — unlike the
//! TUI, which prints inline and scrolls like a log, this surface owns the
//! whole screen: a tree pane on the left (the workspace's git marks riding
//! each file), a reading pane on the right, and a status bar. The tree is
//! built by a plain recursive walk that skips `.git`/`target`/`node_modules`
//! and is bounded by a node budget; the git marks come from the tools
//! crate's [`GitStatusTool`] — the same status the agent sees. Selection
//! rides in inverse video, dirs collapse with `↵`, files open read-only.
//!
//! **Degraded shapes** (all documented, all honest):
//! - *Piped* (`amparo code DIR < /dev/null`): a plain-text report — the
//!   tree plus the git summary, zero escapes.
//! - *NO_COLOR*: the same surface without color (selection inverse stays —
//!   an attribute, not a color).
//! - *Windows*: no raw mode — the piped report prints, with a stderr hint
//!   when the operator asked for interactive.
//!
//! **Known simplifications, kept deliberate**: the walk is shallow-metadata
//! (symlinks never follow — no cycles); files cap at 1 MiB when opened and
//! binary files show a summary row instead of mojibake; resize is picked up
//! on the 0.1s read timeout, not by signal. Scientific voice, `[tag]`
//! lines, `—` in copy, no emoji.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use amparo_tools::git::GitStatusTool;
use amparo_tools::{PathPolicy, ToolCall, ToolExecutor};
use serde_json::json;

#[cfg(unix)]
use crate::raw::{enter_raw_mode, read_byte, term_size};

const CODE_USAGE: &str = "\
amparo code — the coding terminal

USAGE:
  amparo code [DIR]

ARGS:
  DIR   the tree root (default: the workspace root)

On a unix terminal this opens an alternate-screen file tree with the
workspace's git marks: ↑↓ move, ↵ open a file or toggle a directory,
r rescan, q quit. Piped, it prints a plain-text report — the tree plus
the git summary, zero escapes.";

/// The node budget for one tree build — a runaway directory (a mount
/// point, a cache) can never make the surface hang or the frame enormous.
const MAX_TREE_NODES: usize = 5000;

/// The most of a file the reading pane holds — the rest is truncated,
/// announced, not silently dropped.
const MAX_FILE_BYTES: usize = 1 << 20;

/// Directories the tree always skips. `.git` is a status source, not
/// content; `target`/`node_modules` are build caches.
const IGNORED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

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
#[derive(Debug, PartialEq, Eq)]
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
    match read_byte(stdin) {
        Some(b) => decode_key(b, &mut || read_byte(stdin)),
        None => Key::Tick,
    }
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

    /// Applies one key; returns true when the frame changed.
    fn handle_key(&mut self, k: Key, root: &Path, h: usize) -> bool {
        let body_h = h.saturating_sub(2).max(1);
        match &mut self.view {
            Some(v) => match k {
                Key::Up => {
                    v.scroll = v.scroll.saturating_sub(1);
                    true
                }
                Key::Down => {
                    v.scroll = (v.scroll + 1).min(v.lines.len().saturating_sub(1));
                    true
                }
                Key::Home => {
                    v.scroll = 0;
                    true
                }
                Key::End => {
                    v.scroll = v.lines.len().saturating_sub(body_h);
                    true
                }
                Key::PageUp => {
                    v.scroll = v.scroll.saturating_sub(body_h);
                    true
                }
                Key::PageDown => {
                    v.scroll = (v.scroll + body_h).min(v.lines.len().saturating_sub(body_h));
                    true
                }
                Key::Esc | Key::Quit => {
                    self.view = None;
                    true
                }
                _ => false,
            },
            None => match k {
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
            // Right: the opened file, or the hint.
            let right = match (&state.view, row_idx) {
                (Some(v), _) => {
                    let idx = v.scroll + r;
                    match v.lines.get(idx) {
                        Some(content) => {
                            let num = paint.apply(Ink::Dim, &format!("{:>4} ", idx + 1));
                            format!("{num}{}", clip(content, pane_w.saturating_sub(5)))
                        }
                        None => String::new(),
                    }
                }
                (None, 0) => paint.apply(Ink::Dim, "↵ open a file — it opens here"),
                (None, 1) => {
                    let f = &state.flat[state.selected()];
                    paint.apply(
                        Ink::Dim,
                        if f.is_dir {
                            "↵ toggles a directory"
                        } else {
                            "↵ opens the selected file read-only"
                        },
                    )
                }
                (None, 2) => paint.apply(Ink::Dim, "↑↓ move · r rescan · q quit"),
                (None, _) => String::new(),
            };
            line.push_str(&clip_painted(&right, pane_w));
            line.push_str("\x1b[K");
        }
        out.push_str(&line);
        out.push_str("\r\n");
    }

    // Status bar, inverse across the full width.
    let legend = match &state.view {
        Some(v) => {
            let trunc = if v.truncated { " · truncated" } else { "" };
            format!(
                "viewing {} — line {}/{} · ↑↓ scroll · esc back · q quit{trunc}",
                v.rel,
                v.scroll + 1,
                v.lines.len().max(1)
            )
        }
        None => "↑↓ move · ↵ open · r rescan · q quit".to_string(),
    };
    let left = clip(
        &format!("amparo code — {}", root.display()),
        w.saturating_sub(legend.chars().count() + 3).max(8),
    );
    let status = format!(" {left} · {legend}");
    let bar = format!("{status}{}", " ".repeat(w.saturating_sub(status.chars().count())));
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

    let result = (|| -> Result<(), String> {
        loop {
            if let Some(size) = term_size() {
                if size != last_size {
                    last_size = size;
                    dirty = true;
                }
            }
            if dirty {
                let (w, h) = last_size;
                let frame = draw(&state, &root, branch.as_deref(), &paint, w, h);
                out.write_all(frame.as_bytes()).map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                dirty = false;
            }
            match next_key(&mut stdin) {
                Key::Tick => continue, // the size probe above ran; nothing else
                Key::Quit => {
                    if state.view.is_none() {
                        break; // q quits from the tree, backs out of a file
                    }
                    state.view = None;
                    dirty = true;
                }
                Key::Rescan => {
                    // Fresh marks + a fresh tree — the workspace moved
                    // under us. Blocking the reader thread on the handle
                    // is fine: this is the only interactive task.
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
                k => {
                    if state.handle_key(k, &root, last_size.1) {
                        state.follow(last_size.1.saturating_sub(2).max(1));
                        dirty = true;
                    }
                }
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
        };
        // Move the selection to main.rs (root, src, main.rs in preorder).
        state.sel = 2;
        assert!(state.handle_key(Key::Enter, &root, 24));
        let view = state.view.expect("view opened");
        assert_eq!(view.rel, "src/main.rs");
        assert_eq!(view.lines, vec!["fn main() {}"]);
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
        let frame = draw(&state, &root, None, &paint, 80, 24);
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
}
