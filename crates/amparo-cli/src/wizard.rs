//! `amparo wizard` — the first-run profile wizard (#167).
//!
//! Local-first: four ruled steps — workspace → LLM endpoint → optional
//! Guardrail policy (wire check URL, key, console URL) → optional Engram
//! Vault memory URL — captured line by line (Enter skips; piped stdin
//! answers one line per prompt, in order) and written to
//! `{workspace}/.amparo/profile.json`, mode 0600 — the profile can carry
//! keys, so it is never world-readable. Each policy/memory step prints a
//! delegation line: the sibling CLI on PATH pairs this machine
//! (`guardrail link`, `engram pair`), a missing one gets its install
//! one-liner.
//!
//! Every later wire reads the profile to fill environment gaps: the
//! environment wins, the profile fills what is unset. `amparo tui` boots
//! into the wizard when no LLM is configured anywhere (env or profile);
//! `amparo run` never starts it — the run surface keeps its fail-fast
//! environment error. The wizard ends with a boot-banner preview built
//! from the answers: the same five lines the next boot greets with.

use crate::run::site_desc;
use amparo_tools::PathPolicy;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// The first-run profile: the wizard's captured answers, one optional
/// field per prompt. `None` means "skipped" — the environment's value
/// (if any) stands.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Profile {
    /// The BYO-LLM base URL (`AMPARO_INFERENCE_URL`).
    #[serde(default)]
    pub inference_url: Option<String>,
    /// The default model id (`AMPARO_INFERENCE_MODEL`).
    #[serde(default)]
    pub inference_model: Option<String>,
    /// The API key (`AMPARO_INFERENCE_KEY`); never echoed back.
    #[serde(default)]
    pub inference_key: Option<String>,
    /// `openai` (default) or `anthropic` (`AMPARO_INFERENCE_PROVIDER`).
    #[serde(default)]
    pub inference_provider: Option<String>,
    /// The Guardrail Console wire-protocol engine URL — the `--policy-url`
    /// fallback, never under `--allow-all`.
    #[serde(default)]
    pub policy_url: Option<String>,
    /// The policy key (`AMPARO_POLICY_KEY`); never echoed back.
    #[serde(default)]
    pub policy_key: Option<String>,
    /// The Guardrail Console URL (`AMPARO_CONSOLE_POLICY_URL`) — where the
    /// TUI's `/policy` commands write org rules. Falls back to
    /// [`amparo_tools::DEFAULT_CONSOLE_POLICY_URL`] when unset.
    #[serde(default)]
    pub console_url: Option<String>,
    /// The Engram Vault daemon (engramd) base URL (`AMPARO_ENGRAM_URL`, with
    /// `AMPARO_MEMORY_BACKEND=engram`).
    #[serde(default)]
    pub memory_url: Option<String>,
    /// The engramd key (`AMPARO_ENGRAM_KEY`); never echoed back.
    #[serde(default)]
    pub memory_key: Option<String>,
}

/// The workspace root every wire starts from — `AMPARO_WORKSPACE` or the
/// documented default. The wizard's workspace step may choose a
/// different one; callers decide whether to honor it.
pub(crate) fn workspace_root() -> PathBuf {
    PathPolicy::from_env().workspace_root
}

/// The profile path under a workspace root.
fn profile_path(root: &Path) -> PathBuf {
    root.join(".amparo").join("profile.json")
}

/// Loads the profile under `root`. Missing → `None` (not an error);
/// unreadable or malformed → one `[amparo-cli]` warning and `None` —
/// a broken profile must never block a run, the environment may still
/// be complete.
pub(crate) fn load(root: &Path) -> Option<Profile> {
    let path = profile_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("[amparo-cli] cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(profile) => Some(profile),
        Err(e) => {
            tracing::warn!(
                "[amparo-cli] ignoring malformed profile {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Fills environment gaps from the profile: each captured answer lands
/// only where the variable is unset. The environment always wins — a
/// profile can repair, never override.
pub(crate) fn fill_env_gaps(profile: &Profile) {
    let set = |key: &str, value: &Option<String>| {
        if std::env::var_os(key).is_none() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
    };
    set("AMPARO_INFERENCE_URL", &profile.inference_url);
    set("AMPARO_INFERENCE_MODEL", &profile.inference_model);
    set("AMPARO_INFERENCE_KEY", &profile.inference_key);
    set("AMPARO_INFERENCE_PROVIDER", &profile.inference_provider);
    set("AMPARO_POLICY_KEY", &profile.policy_key);
    set("AMPARO_CONSOLE_POLICY_URL", &profile.console_url);
    if std::env::var_os("AMPARO_MEMORY_BACKEND").is_none() && profile.memory_url.is_some() {
        std::env::set_var("AMPARO_MEMORY_BACKEND", "engram");
    }
    set("AMPARO_ENGRAM_URL", &profile.memory_url);
    set("AMPARO_ENGRAM_KEY", &profile.memory_key);
}

// ---------------------------------------------------------------------------
// Sibling CLI delegation (M12 W2)
// ---------------------------------------------------------------------------

/// The Guardrail install one-liner shown when the `guardrail` CLI is not
/// on PATH — `guardrail link` pairs this machine with a gk_ org key
/// (verified 2026-09-01).
pub(crate) const GUARDRAIL_INSTALL_HINT: &str =
    "curl -fsSL https://downloads.ellmstack.dev/install.sh | bash";

/// The Engram install one-liner shown when the `engram` CLI is not on
/// PATH — `engram pair` pairs this machine with the memory daemon
/// (verified 2026-09-01).
pub(crate) const ENGRAM_INSTALL_HINT: &str =
    "curl -fsSL https://engram.ellmstack.dev/install.sh | bash";

/// True when `name` resolves to an executable on PATH — the check behind
/// the wizard's delegation lines: a found sibling CLI delegates setup to
/// its own flow (`guardrail link`, `engram pair`), a missing one gets the
/// install one-liner. Windows also probes `.exe`/`.cmd`/`.bat` suffixes.
pub(crate) fn command_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suffix in suffixes {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() && executable(&candidate) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> bool {
    // Windows has no mode bits — `is_file` on the probed suffix is the
    // whole check.
    true
}

/// One sibling-CLI delegation line: a found CLI delegates setup to its own
/// flow (`guardrail link`, `engram pair`), a missing one gets the install
/// one-liner. A pure helper so both branches are unit-pinned without
/// touching PATH.
fn delegation_line(cli: &str, pairing: &str, install: &str, on_path: bool) -> String {
    if on_path {
        format!("[wizard] found the {cli} CLI — {pairing}")
    } else {
        format!("[wizard] no {cli} CLI on PATH — install one with: {install}")
    }
}

/// Whether both required inference variables are missing — the wizard's
/// trigger. Present-but-invalid values are left alone: the wire's own
/// error names them far better than a first-run prompt would.
pub(crate) fn inference_unset() -> bool {
    std::env::var_os("AMPARO_INFERENCE_URL").is_none()
        || std::env::var_os("AMPARO_INFERENCE_MODEL").is_none()
}

/// Writes the profile, mode 0600 — it can carry keys.
pub(crate) fn save(root: &Path, profile: &Profile) -> Result<PathBuf, String> {
    let dir = root.join(".amparo");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("profile.json");
    let json = serde_json::to_string_pretty(profile)
        .map_err(|e| format!("cannot encode the profile: {e}"))?;
    write_0600(&path, json.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
fn write_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Windows has no mode bits — the directory-level ACLs stand.
    std::fs::write(path, bytes)
}

/// The policy segment in the banner's grammar, from the answers.
pub(crate) fn policy_desc(profile: &Profile) -> String {
    match &profile.policy_url {
        Some(url) => format!("wire {}", site_desc(url)),
        None => "deny-all (no --policy-url or --allow-all)".to_string(),
    }
}

/// The memory segment in the banner's grammar, from the answers.
pub(crate) fn memory_desc(profile: &Profile) -> String {
    match &profile.memory_url {
        Some(url) => format!("Engram Vault @ {}", site_desc(url)),
        None => "built-in store".to_string(),
    }
}

/// The console line in the wizard summary — the effective console host,
/// never a key-carrying URL. `None` when no console is configured (the
/// client falls back to [`amparo_tools::DEFAULT_CONSOLE_POLICY_URL`]).
pub(crate) fn console_desc(profile: &Profile) -> Option<String> {
    profile.console_url.as_deref().map(|url| {
        format!(
            "[policy] console {} — /policy commands write org rules there",
            site_desc(url)
        )
    })
}

/// The infer line in the banner's grammar, from the answers — the host
/// in the ledger's `scheme://host[:port]` shape, never a key-carrying
/// URL.
pub(crate) fn infer_desc(profile: &Profile) -> String {
    match (&profile.inference_url, &profile.inference_model) {
        (Some(url), Some(model)) => format!(
            "{} · {} · {}",
            profile.inference_provider.as_deref().unwrap_or("openai"),
            model,
            site_desc(url)
        ),
        _ => "not configured — set AMPARO_INFERENCE_URL and AMPARO_INFERENCE_MODEL".to_string(),
    }
}

/// The one-line chain the next boot will enforce, from the answers. The
/// ceiling is the flag default — the wizard captures configuration, the
/// run's flags still decide the run.
pub(crate) fn chain_line(profile: &Profile) -> String {
    format!(
        "registry → trust ceiling (system_control) → policy ({}) → human approval (terminal y/N, 60s fail-closed)",
        policy_desc(profile)
    )
}

/// One prompt: print, then read one line. EOF is an error — the wizard
/// must not half-capture a profile from a silent pipe.
fn ask(reader: &mut dyn BufRead, prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Err("the wizard needs answers — one line per prompt (Enter skips)".to_string()),
        Ok(_) => Ok(line.trim().to_string()),
        Err(e) => Err(format!("cannot read the wizard answers: {e}")),
    }
}

/// An empty answer keeps the fallback; a non-empty one replaces it.
fn pick(answer: String, fallback: Option<String>) -> Option<String> {
    if answer.is_empty() {
        fallback
    } else {
        Some(answer)
    }
}

/// A prompt's default: the environment's value when set, else the
/// existing profile's — re-runs repair a profile instead of blanking it.
fn env_or(
    existing: &Option<Profile>,
    from_profile: impl Fn(&Profile) -> Option<String>,
    env_key: &str,
) -> Option<String> {
    std::env::var(env_key)
        .ok()
        .or_else(|| existing.as_ref().and_then(from_profile))
}

/// The wizard itself: four ruled steps, one line per prompt, then the
/// profile (0600), the `[chain]` summary and the boot-banner preview.
/// Returns the chosen workspace root and the captured profile.
pub(crate) fn run(
    reader: &mut dyn BufRead,
    default_root: &Path,
) -> Result<(PathBuf, Profile), String> {
    // Re-runs use the existing profile's values as defaults.
    let existing = load(default_root);
    println!("Greetings! My name is Amparo, built by EL AI Intelligence.");
    println!("[wake] first run — four steps and you're awake: workspace, LLM, policy, memory.");
    println!("[wizard] policy and memory are recommended, never required — Enter skips any line.");
    println!("[wizard] keys you type here are never echoed back.");

    println!();
    println!("▐ step 1/4 — workspace");
    println!("  where Amparo lives: the profile, ledger and sessions root");
    let workspace = ask(
        reader,
        &format!("  workspace [default {}] › ", default_root.display()),
    )?;
    let root = if workspace.is_empty() {
        default_root.to_path_buf()
    } else {
        let chosen = PathBuf::from(workspace);
        if chosen.is_absolute() {
            chosen
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(chosen))
                .map_err(|e| format!("cannot resolve the workspace: {e}"))?
        }
    };
    if root != default_root {
        println!(
            "[wizard] later runs root at {} — set AMPARO_WORKSPACE (or pass --workspace) to keep them together",
            root.display()
        );
    }

    let url_default = env_or(
        &existing,
        |p| p.inference_url.clone(),
        "AMPARO_INFERENCE_URL",
    );
    let model_default = env_or(
        &existing,
        |p| p.inference_model.clone(),
        "AMPARO_INFERENCE_MODEL",
    );
    let provider_default = env_or(
        &existing,
        |p| p.inference_provider.clone(),
        "AMPARO_INFERENCE_PROVIDER",
    );
    println!();
    println!("▐ step 2/4 — LLM endpoint");
    println!("  a BYO-LLM URL and model (openai or anthropic wire format)");
    let url = ask(
        reader,
        &format!("  url [{}] › ", url_default.as_deref().unwrap_or("none")),
    )?;
    let model = ask(
        reader,
        &format!(
            "  model [{}] › ",
            model_default.as_deref().unwrap_or("none")
        ),
    )?;
    let provider = ask(
        reader,
        &format!(
            "  provider [default {}] › ",
            provider_default.as_deref().unwrap_or("openai")
        ),
    )?;
    let key_hint = if existing
        .as_ref()
        .and_then(|p| p.inference_key.as_ref())
        .is_some()
    {
        " (set)"
    } else {
        " (empty = keyless)"
    };
    let key = ask(reader, &format!("  key{key_hint} › "))?;

    let policy_default = existing.as_ref().and_then(|p| p.policy_url.clone());
    let policy_key_default = existing.as_ref().and_then(|p| p.policy_key.clone());
    println!();
    println!("▐ step 3/4 — Guardrail Console policy (recommended, never required)");
    println!("  the wire check URL — routed through the console, org rules apply to every check");
    println!("  the console URL — the TUI's /policy commands write org rules there");
    println!("  keys stay local");
    println!(
        "{}",
        delegation_line(
            "guardrail",
            "`guardrail link` pairs this machine with an org key",
            GUARDRAIL_INSTALL_HINT,
            command_on_path("guardrail")
        )
    );
    let policy_url = ask(
        reader,
        &format!(
            "  engine url [recommended, never required{}] › ",
            if policy_default.is_some() {
                " — set"
            } else {
                ""
            }
        ),
    )?;
    let policy_key = ask(
        reader,
        &format!(
            "  key{} › ",
            if policy_key_default.is_some() {
                " (set)"
            } else {
                " (empty = none)"
            }
        ),
    )?;
    let console_default = std::env::var("AMPARO_CONSOLE_POLICY_URL")
        .ok()
        .or_else(|| existing.as_ref().and_then(|p| p.console_url.clone()));
    let console_url = ask(
        reader,
        &format!(
            "  console url [default {}] › ",
            console_default
                .as_deref()
                .unwrap_or(amparo_tools::DEFAULT_CONSOLE_POLICY_URL)
        ),
    )?;

    let memory_default = std::env::var("AMPARO_ENGRAM_URL")
        .ok()
        .or_else(|| existing.as_ref().and_then(|p| p.memory_url.clone()));
    let memory_key_default = existing.as_ref().and_then(|p| p.memory_key.clone());
    println!();
    println!("▐ step 4/4 — Engram Vault memory (recommended, never required)");
    println!("  an Engram Vault daemon (engramd) URL — memories that outlive the session");
    println!(
        "{}",
        delegation_line(
            "engram",
            "`engram pair` pairs this machine with the memory daemon",
            ENGRAM_INSTALL_HINT,
            command_on_path("engram")
        )
    );
    let memory_url = ask(
        reader,
        &format!(
            "  url [recommended, never required{}] › ",
            if memory_default.is_some() {
                " — set"
            } else {
                ""
            }
        ),
    )?;
    let memory_key = ask(
        reader,
        &format!(
            "  key{} › ",
            if memory_key_default.is_some() {
                " (set)"
            } else {
                " (empty = none)"
            }
        ),
    )?;

    // Skipped answers keep their defaults; key answers keep the existing
    // profile value (an environment key is never copied silently).
    let profile = Profile {
        inference_url: pick(url, url_default),
        inference_model: pick(model, model_default),
        inference_key: pick(key, existing.as_ref().and_then(|p| p.inference_key.clone())),
        inference_provider: pick(provider, provider_default),
        policy_url: pick(policy_url, policy_default),
        policy_key: pick(policy_key, policy_key_default),
        console_url: pick(console_url, console_default),
        memory_url: pick(memory_url, memory_default),
        memory_key: pick(memory_key, memory_key_default),
    };

    let path = save(&root, &profile)?;
    println!();
    println!("[wizard] profile written to {} (mode 0600)", path.display());
    println!("[chain] {}", chain_line(&profile));
    println!("[infer] {}", infer_desc(&profile));
    println!("[memory] {}", memory_desc(&profile));
    if let Some(line) = console_desc(&profile) {
        println!("{line}");
    }
    println!();
    println!("[wizard] the next boot greets with:");
    println!();
    println!("Greetings! My name is Amparo, built by EL AI Intelligence.");
    println!("[wake] Amparo is awake.");
    println!("[gate] chain: {}", chain_line(&profile));
    println!("[infer] {}", infer_desc(&profile));
    println!("[memory] {}", memory_desc(&profile));
    Ok((root, profile))
}

/// The TUI boot's first-run path: load + fill; when the required pair is
/// still missing, run the wizard and honor its workspace for the rest of
/// this process. The interactive boot only — piped mode never starts the
/// wizard, it would eat the task stream.
pub(crate) fn ensure_configured() -> Result<(), String> {
    let root = workspace_root();
    if let Some(profile) = load(&root) {
        fill_env_gaps(&profile);
    }
    if !inference_unset() {
        return Ok(());
    }
    let (root, profile) = run(&mut std::io::stdin().lock(), &root)?;
    std::env::set_var("AMPARO_WORKSPACE", &root);
    fill_env_gaps(&profile);
    Ok(())
}

/// The `amparo wizard --help` copy.
const WIZARD_USAGE: &str = "\
amparo wizard — the first-run profile wizard

USAGE:
  amparo wizard

Four steps — workspace → LLM endpoint → optional Guardrail policy (wire
check URL, key, console URL) → optional Engram Vault memory URL — written
to {workspace}/.amparo/profile.json (mode 0600). Policy and memory are
recommended, never required; Enter skips any line; keys are never echoed.
Piped stdin answers one line per prompt, in order: workspace, url, model,
provider, key, policy url, policy key, console url, memory url, memory key.
Steps 3 and 4 note the sibling CLI (`guardrail link`, `engram pair`) or
its install one-liner.

The profile fills environment gaps on every later run (the environment
wins). The TUI boots into the wizard when no LLM is configured anywhere;
`amparo run` keeps its fail-fast environment error instead.";

/// The `amparo wizard` entry point — no flags, one interactive (or
/// piped) capture. Exit codes: 0 captured, 1 capture failed, 2 usage.
pub(crate) async fn dispatch(args: impl Iterator<Item = String>) {
    let args: Vec<String> = args.collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("--help") | Some("-h") => {
            println!("{WIZARD_USAGE}");
            return;
        }
        Some(other) => {
            eprintln!("amparo wizard takes no arguments (got {other}); see `amparo wizard --help`");
            std::process::exit(2);
        }
    }
    let root = workspace_root();
    if let Err(message) = run(&mut std::io::stdin().lock(), &root) {
        eprintln!("amparo wizard: {message}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Profile {
        Profile {
            inference_url: Some("http://127.0.0.1:11434/v1".to_string()),
            inference_model: Some("qwen2.5:14b".to_string()),
            inference_key: Some("secret".to_string()),
            inference_provider: Some("openai".to_string()),
            policy_url: Some("http://127.0.0.1:8080".to_string()),
            policy_key: Some("gk_test".to_string()),
            memory_url: Some("http://127.0.0.1:8787".to_string()),
            memory_key: Some("mem_key".to_string()),
            console_url: Some("http://127.0.0.1:9101".to_string()),
        }
    }

    #[test]
    fn profile_round_trips() {
        let json = serde_json::to_string(&full()).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.inference_url, full().inference_url);
        assert_eq!(back.inference_model, full().inference_model);
        assert_eq!(back.inference_key, full().inference_key);
        assert_eq!(back.policy_url, full().policy_url);
        assert_eq!(back.console_url, full().console_url);
        assert_eq!(back.memory_url, full().memory_url);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let profile: Profile =
            serde_json::from_str(r#"{"inference_url":"http://x/v1","future_field":1}"#).unwrap();
        assert_eq!(profile.inference_url.as_deref(), Some("http://x/v1"));
        assert!(profile.inference_model.is_none());
    }

    #[test]
    fn chain_line_names_every_segment() {
        let wired = chain_line(&full());
        assert!(
            wired.contains("registry → trust ceiling (system_control)"),
            "{wired}"
        );
        assert!(
            wired.contains("policy (wire http://127.0.0.1:8080)"),
            "{wired}"
        );
        assert!(
            wired.contains("human approval (terminal y/N, 60s fail-closed)"),
            "{wired}"
        );
        let bare = chain_line(&Profile::default());
        assert!(
            bare.contains("policy (deny-all (no --policy-url or --allow-all))"),
            "{bare}"
        );
    }

    #[test]
    fn descs_never_echo_keys() {
        let mut profile = full();
        profile.inference_url = Some("http://user:pass@host:8080/v1".to_string());
        let infer = infer_desc(&profile);
        assert!(infer.contains("host:8080"), "{infer}");
        assert!(!infer.contains("pass"), "{infer}");
        assert!(!infer.contains("secret"), "{infer}");
        let mut policy = full();
        policy.policy_url = Some("http://gk_secret@127.0.0.1:8080".to_string());
        let desc = policy_desc(&policy);
        assert!(desc.contains("127.0.0.1:8080"), "{desc}");
        assert!(!desc.contains("gk_secret"), "{desc}");
        let mut console = full();
        console.console_url = Some("http://gk_console_secret@console:8080".to_string());
        let line = console_desc(&console).unwrap();
        assert!(line.contains("console:8080"), "{line}");
        assert!(!line.contains("gk_console_secret"), "{line}");
        assert!(console_desc(&Profile::default()).is_none());
    }

    #[test]
    fn scripted_stdin_captures_the_profile() {
        let root = std::env::temp_dir().join(format!("amparo-wizard-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Ten answers: workspace, url, model, provider, key, policy url,
        // policy key, console url, memory url, memory key.
        let mut stdin: &[u8] = b"/tmp/unused-workspace\nhttp://127.0.0.1:11434/v1\nqwen\n\n\nhttp://127.0.0.1:8080\n\nhttp://127.0.0.1:9101\n\n\n";
        let (chosen, profile) = run(&mut stdin, &root).unwrap();
        // The wizard absolutizes the workspace answer. `/tmp/…` is
        // already absolute on Unix and stays verbatim; on Windows it is
        // root-relative, so the runner's drive prefix is joined on
        // (`D:/tmp/…`) — the contract is an absolute path carrying the
        // name, not the literal string.
        assert!(chosen.is_absolute(), "workspace must be absolute: {chosen:?}");
        assert_eq!(
            chosen.file_name().and_then(|n| n.to_str()),
            Some("unused-workspace"),
            "the workspace name is captured: {chosen:?}"
        );
        assert_eq!(
            profile.inference_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(profile.inference_model.as_deref(), Some("qwen"));
        assert_eq!(profile.policy_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(
            profile.console_url.as_deref(),
            Some("http://127.0.0.1:9101")
        );
        assert!(profile.memory_url.is_none());
        let path = profile_path(&chosen);
        let back: Profile = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.inference_model.as_deref(), Some("qwen"));
        assert_eq!(
            back.console_url.as_deref(),
            Some("http://127.0.0.1:9101")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the profile must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&chosen);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn eof_is_an_error_not_a_half_profile() {
        let mut stdin: &[u8] = b"";
        let root = std::env::temp_dir().join(format!("amparo-wizard-eof-{}", std::process::id()));
        let err = run(&mut stdin, &root).unwrap_err();
        assert!(err.contains("needs answers"), "{err}");
    }

    #[test]
    fn delegation_line_pins_both_branches() {
        let found = delegation_line(
            "guardrail",
            "`guardrail link` pairs this machine with an org key",
            GUARDRAIL_INSTALL_HINT,
            true,
        );
        assert!(
            found.contains(
                "[wizard] found the guardrail CLI — `guardrail link` pairs this machine with an org key"
            ),
            "{found}"
        );
        let missing = delegation_line(
            "engram",
            "`engram pair` pairs this machine with the memory daemon",
            ENGRAM_INSTALL_HINT,
            false,
        );
        assert!(
            missing.contains("[wizard] no engram CLI on PATH — install one with: "),
            "{missing}"
        );
        assert!(missing.contains(ENGRAM_INSTALL_HINT), "{missing}");
    }

    #[test]
    fn command_on_path_finds_executable_files_only() {
        let dir = std::env::temp_dir().join(format!("amparo-on-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exe = dir.join("fake-guardrail");
            std::fs::write(&exe, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            let plain = dir.join("fake-engram");
            std::fs::write(&plain, "not executable").unwrap();
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
            // Keep the system PATH after the temp dir so nothing else in
            // this process loses its binaries while the probe runs.
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
            assert!(command_on_path("fake-guardrail"));
            assert!(!command_on_path("fake-engram"));
        }
        #[cfg(not(unix))]
        {
            std::fs::write(dir.join("fake-guardrail.exe"), "x").unwrap();
            std::env::set_var(
                "PATH",
                format!(
                    "{};{}",
                    dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
            assert!(command_on_path("fake-guardrail"));
        }
        assert!(!command_on_path("never-installed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
