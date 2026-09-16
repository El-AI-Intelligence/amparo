//! `amparo wizard` — the first-run profile wizard (#167).
//!
//! Local-first: four ruled steps — workspace → provider → optional
//! Guardrail policy (wire check URL, key, console URL) → optional Engram
//! Vault memory URL — captured line by line (Enter skips; piped stdin
//! answers one line per prompt, in order) and written to
//! `{workspace}/.amparo/profile.json`, mode 0600 — the profile can carry
//! keys, so it is never world-readable. The policy and memory steps are
//! optional: their URL prompts validate (a command-shaped answer like
//! `engram pair` gets the run-it-in-another-window hint instead of being
//! saved), and Enter skips them entirely.
//!
//! The provider step is a picker over [`catalog::CATALOG`] — a number or
//! any catalog id/alias (`kimi`, `claude`, `ollama`, …) resolves to a
//! spec whose endpoint and model prefill the following prompts. The URL
//! is validated before it is accepted (scheme + host, normalized — a
//! junk URL re-prompts instead of saving), and once the profile is
//! written a short live probe runs through the resolved provider
//! (best-effort: a failure prints the reason and how to recover, never
//! blocks). The probe is skipped on piped stdin, keeping the scripted
//! path fast and offline.
//!
//! Every later wire reads the profile to fill environment gaps: the
//! environment wins, the profile fills what is unset. `amparo tui` boots
//! into the wizard when no LLM is configured anywhere (env or profile);
//! `amparo run` never starts it — the run surface keeps its fail-fast
//! environment error. The wizard ends with a boot-banner preview built
//! from the answers: the same five lines the next boot greets with.

use crate::run::site_desc;
use amparo_inference::{
    catalog, CloudConfig, InferenceConfig, InferenceProvider, InferenceRequest,
};
use amparo_tools::PathPolicy;
use std::future::Future;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The infer line in the banner's grammar, from the answers — the
/// catalog label (resolved from the stored provider id), the model, and
/// the host in the ledger's `scheme://host[:port]` shape, never a
/// key-carrying URL.
pub(crate) fn infer_desc(profile: &Profile) -> String {
    match (&profile.inference_url, &profile.inference_model) {
        (Some(url), Some(model)) => {
            let provider = profile.inference_provider.as_deref().unwrap_or("openai");
            let label = catalog::lookup(provider)
                .map(|spec| spec.label)
                .unwrap_or(provider);
            format!("{label} · {model} · {}", site_desc(url))
        }
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

/// The provider picker: a numbered friendly list over the catalog.
/// A line may be a 1-based number or any catalog id/alias
/// (case-insensitive — `kimi`, `claude`, `ollama`, …); Enter keeps the
/// default. Loops until a line resolves.
fn pick_provider(
    reader: &mut dyn BufRead,
    default: &catalog::ProviderSpec,
) -> Result<&'static catalog::ProviderSpec, String> {
    for (index, spec) in catalog::CATALOG.iter().enumerate() {
        println!("  {:>2}  {}", index + 1, spec.label);
    }
    loop {
        let answer = ask(
            reader,
            &format!("  pick a number or a name [{}] › ", default.label),
        )?;
        let answer = if answer.is_empty() {
            default.id.to_string()
        } else {
            answer
        };
        if let Ok(number) = answer.trim().parse::<usize>() {
            if let Some(spec) = catalog::CATALOG.get(number.wrapping_sub(1)) {
                return Ok(spec);
            }
        }
        if let Some(spec) = catalog::lookup(&answer) {
            return Ok(spec);
        }
        println!(
            "[wizard] hmm, '{}' isn't in the list — a number (1–{}) or a name like 'kimi' works",
            answer.trim(),
            catalog::CATALOG.len()
        );
    }
}

/// The URL prompt: prefilled, editable, and validated before it is
/// accepted. Enter keeps a valid default; a non-empty answer (or a
/// default that no longer normalizes) must be `http://`/`https://` with
/// a host — anything else re-prompts with a hint instead of saving junk.
fn ask_url(reader: &mut dyn BufRead, default: Option<String>) -> Result<String, String> {
    // A saved value that fails validation is repaired here, not echoed
    // back as a tempting Enter.
    if let Some(saved) = &default {
        if catalog::normalize_base_url(saved).is_err() {
            println!("[wizard] the saved URL '{saved}' doesn't look valid — please type it again");
        }
    }
    let default = default.filter(|saved| catalog::normalize_base_url(saved).is_ok());
    loop {
        let hint = default.as_deref().unwrap_or("required");
        let answer = ask(reader, &format!("  url [{hint}] › "))?;
        if answer.is_empty() {
            if let Some(saved) = &default {
                // Pre-checked above — the expect cannot fire.
                return Ok(catalog::normalize_base_url(saved).expect("pre-checked URL"));
            }
            println!("[wizard] a URL is needed — e.g. https://api.moonshot.cn/v1");
            continue;
        }
        match catalog::normalize_base_url(&answer) {
            Ok(url) => return Ok(url),
            Err(err) => println!("[wizard] that doesn't look right — {err}"),
        }
    }
}

/// The optional URL prompts (policy, console, memory): validation like
/// [`ask_url`], except Enter means "skip this one" — the step is
/// optional, and the caller's fallback (env, existing profile, built-in)
/// stands. A command-shaped answer (`engram pair`, `guardrail link`, …)
/// gets the run-it-in-another-window hint instead of being saved as a
/// URL — the exact trap the first drill hit.
fn ask_optional_url(reader: &mut dyn BufRead, default: Option<String>) -> Result<String, String> {
    // A saved value that fails validation is repaired here — it must
    // never be offered back as a tempting Enter.
    if let Some(saved) = &default {
        if catalog::normalize_base_url(saved).is_err() {
            println!(
                "[wizard] the saved value '{saved}' isn't a URL — type the real one, or press Enter to drop it"
            );
        }
    }
    let default = default.filter(|saved| catalog::normalize_base_url(saved).is_ok());
    loop {
        let hint = default.as_deref().unwrap_or("none");
        let answer = ask(reader, &format!("  url [Enter skips — {hint}] › "))?;
        if answer.is_empty() {
            return Ok(String::new());
        }
        if answer.split_whitespace().count() > 1 {
            println!(
                "[wizard] that looks like a command, not a URL — run it in another window, then paste the URL it prints (e.g. http://127.0.0.1:8787). Enter skips."
            );
            continue;
        }
        match catalog::normalize_base_url(&answer) {
            Ok(url) => return Ok(url),
            Err(err) => println!(
                "[wizard] that doesn't look like a URL — it needs http(s):// and a host ({err}). Enter skips."
            ),
        }
    }
}

/// One best-effort sniff for an already-installed local model: Ollama
/// (`GET {url}/api/tags`) and LM Studio (`GET {url}/v1/models`) list
/// their models without a key, so the wizard can prefill what the user
/// actually has instead of asking for a catalog id that may not exist.
/// Silent — an offline server or a different port simply leaves the
/// catalog default in place. The ask is one fast GET with a short
/// timeout so the wizard never stalls on it.
fn sniff_local_model(url: &str, provider_id: &str) -> Option<String> {
    let (path, pointer): (&str, &str) = match provider_id {
        "ollama" => ("/api/tags", "/models/0/name"),
        "lmstudio" => ("/v1/models", "/data/0/id"),
        _ => return None,
    };
    let endpoint = format!("{}{path}", url.trim_end_matches('/'));
    let fut = async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .ok()?;
        let value: serde_json::Value =
            client.get(&endpoint).send().await.ok()?.json().await.ok()?;
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    run_short("the local model check", Duration::from_secs(4), fut)
        .ok()
        .flatten()
}

/// The model prompt: prefilled, editable; Enter keeps the default, and
/// a provider without one (OpenRouter, Ollama, LM Studio, custom)
/// requires a typed model id.
fn ask_model(reader: &mut dyn BufRead, default: Option<String>) -> Result<String, String> {
    loop {
        let hint = default.as_deref().unwrap_or("required");
        let answer = ask(reader, &format!("  model [{hint}] › "))?;
        if answer.is_empty() {
            if let Some(saved) = &default {
                return Ok(saved.clone());
            }
            println!("[wizard] this provider needs a model id — e.g. llama3.1:8b");
            continue;
        }
        return Ok(answer);
    }
}

/// The live probe's prompt — short and cheap; any answer is success.
const PROBE_PROMPT: &str = "reply with the single word: ok";

/// The live probe's timeout, in seconds — long enough for a cold local
/// model, short enough to never stall the wizard.
const PROBE_TIMEOUT_SECS: u64 = 20;

/// One live `complete()` through the resolved provider. Returns the
/// answer text; a provider error or a timeout carries the reason. Pure
/// over `&dyn InferenceProvider` — unit-tested with fakes.
async fn probe_provider(
    provider: &dyn InferenceProvider,
    model: &str,
) -> Result<String, String> {
    let request = InferenceRequest {
        prompt: PROBE_PROMPT.to_string(),
        max_tokens: Some(16),
        temperature: Some(0.0),
        model: Some(model.to_string()),
        ..Default::default()
    };
    match tokio::time::timeout(
        Duration::from_secs(PROBE_TIMEOUT_SECS),
        provider.complete(request),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response.text),
        Ok(Err(err)) => Err(format!("the provider said: {err}")),
        Err(_) => Err(format!(
            "no answer within {PROBE_TIMEOUT_SECS}s — check the URL, model and key"
        )),
    }
}

/// The probe's answer, made safe for one display line.
fn probe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Runs `fut` to completion without violating the ambient runtime. The
/// wizard's `run` is sync, but every real call site sits inside the
/// `#[tokio::main]` runtime — building a nested runtime there panics
/// ("Cannot start a runtime from within a runtime"). So: when an ambient
/// runtime exists, the future is spawned onto it and awaited through a
/// channel; outside any runtime (a sync thread of its own), a throwaway
/// current-thread runtime drives the future. A timeout, or the ambient
/// runtime vanishing, maps to an error naming `what`.
fn run_short<T: Send + 'static>(
    what: &str,
    timeout: Duration,
    fut: impl Future<Output = T> + Send + 'static,
) -> Result<T, String> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let (tx, rx) = std::sync::mpsc::channel();
        handle.spawn(async move {
            let _ = tx.send(fut.await);
        });
        match rx.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("{what} timed out after {}s", timeout.as_secs()))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("{what} did not finish — the runtime is gone"))
            }
        }
    } else {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("cannot start {what}: {err}"))?;
        Ok(runtime.block_on(fut))
    }
}

/// The live probe, run after the profile is saved: build the provider
/// from the captured answers and send one short `complete()`. Best-effort
/// — a failure prints the reason and how to recover, and never blocks
/// the wizard. Skipped entirely on piped stdin (the scripted path must
/// stay fast and offline), so the check lives behind the terminal gate
/// at the call site.
fn probe_summary(profile: &Profile) {
    let (Some(url), Some(model)) = (&profile.inference_url, &profile.inference_model) else {
        return;
    };
    let Some(spec) = catalog::lookup(profile.inference_provider.as_deref().unwrap_or("openai"))
    else {
        return;
    };
    let config = InferenceConfig {
        base_url: url.clone(),
        api_key: profile.inference_key.clone().unwrap_or_default(),
        model: model.clone(),
        provider: spec.wire,
        provider_id: Some(spec.id.to_string()),
        timeout_secs: PROBE_TIMEOUT_SECS,
        max_tokens_limit: None,
        allowlist: None,
        cloud: CloudConfig::from_env(),
    };
    let provider = match config.build() {
        Ok(provider) => provider,
        Err(err) => {
            println!("[probe] skipped — {err}");
            return;
        }
    };
    // `run_short` rides the ambient runtime instead of nesting a new one
    // (the old throwaway runtime panicked the whole boot). The probe has
    // its own inner 20s timeout; the outer 30s is the safety net.
    let model_owned = model.clone();
    let outcome = run_short(
        "the probe",
        Duration::from_secs(PROBE_TIMEOUT_SECS + 10),
        async move { probe_provider(provider.as_ref(), &model_owned).await },
    );
    match outcome {
        Ok(Ok(text)) => println!("[probe] ok — {model} answered: {}", probe_text(&text)),
        Ok(Err(reason)) => println!(
            "[probe] couldn't reach the model — {reason} (re-run `amparo wizard` to adjust)"
        ),
        Err(reason) => println!("[probe] skipped — {reason}"),
    }
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
    println!("[wake] first run — four steps and you're awake: workspace, provider, policy, memory.");
    println!("[wizard] policy and memory are recommended, never required — Enter skips any line.");
    println!("[wizard] answers are stored locally (mode 0600) and never printed back.");

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

    let provider_default = env_or(
        &existing,
        |p| p.inference_provider.clone(),
        "AMPARO_INFERENCE_PROVIDER",
    );
    let default_spec = provider_default
        .as_deref()
        .and_then(catalog::lookup)
        .unwrap_or_else(|| catalog::lookup("openai").expect("openai is always cataloged"));
    println!();
    println!("▐ step 2/4 — your AI provider");
    println!("  pick who thinks for you — any of these, or another server");
    println!("  that speaks an OpenAI-style API");
    let spec = pick_provider(reader, default_spec)?;
    println!(
        "[wizard] {} — the endpoint and model below are prefilled, Enter keeps them",
        spec.label
    );

    let url_default = env_or(&existing, |p| p.inference_url.clone(), "AMPARO_INFERENCE_URL")
        .or_else(|| spec.base_url.map(str::to_string));
    let url = ask_url(reader, url_default)?;

    // The catalog default first, then — for the local providers — a
    // silent sniff of what is actually installed (Ollama / LM Studio
    // list their models without a key), so the prefill matches reality
    // instead of a stale catalog id.
    let model_default =
        env_or(&existing, |p| p.inference_model.clone(), "AMPARO_INFERENCE_MODEL")
            .or_else(|| spec.default_model.map(str::to_string))
            .or_else(|| sniff_local_model(&url, spec.id));
    let model = ask_model(reader, model_default)?;

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
    println!("▐ step 3/4 — Guardrail Console policy (optional)");
    println!("  org rules for every check, written through the console — Enter skips, the built-in deny-all stands");
    let policy_url = ask_optional_url(reader, policy_default.clone())?;
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
    let console_url = ask_optional_url(reader, console_default.clone())?;

    let memory_default = std::env::var("AMPARO_ENGRAM_URL")
        .ok()
        .or_else(|| existing.as_ref().and_then(|p| p.memory_url.clone()));
    let memory_key_default = existing.as_ref().and_then(|p| p.memory_key.clone());
    println!();
    println!("▐ step 4/4 — Engram Vault memory (optional)");
    println!("  memories that outlive the session — run `engram pair` in another window, then paste the URL it prints (Enter skips)");
    let memory_url = ask_optional_url(reader, memory_default.clone())?;
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

    // The URL and model were validated (or required) at their prompts;
    // the provider is the resolved catalog id. Skipped answers keep
    // their defaults; key answers keep the existing profile value (an
    // environment key is never copied silently). A saved URL default
    // that no longer validates is dropped here — `ask_optional_url`
    // already warned, and `pick` must not resurrect it on Enter.
    let sane = |value: Option<String>| -> Option<String> {
        value.filter(|v| catalog::normalize_base_url(v).is_ok())
    };
    let profile = Profile {
        inference_url: Some(url),
        inference_model: Some(model),
        inference_key: pick(key, existing.as_ref().and_then(|p| p.inference_key.clone())),
        inference_provider: Some(spec.id.to_string()),
        policy_url: pick(policy_url, sane(policy_default)),
        policy_key: pick(policy_key, policy_key_default),
        console_url: pick(console_url, sane(console_default)),
        memory_url: pick(memory_url, sane(memory_default)),
        memory_key: pick(memory_key, memory_key_default),
    };

    let path = save(&root, &profile)?;
    println!();
    println!("[wizard] profile written to {} (mode 0600)", path.display());
    if std::io::stdin().is_terminal() {
        probe_summary(&profile);
    }
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

Four steps — workspace → provider → optional Guardrail policy (wire
check URL, key, console URL) → optional Engram Vault memory URL — written
to {workspace}/.amparo/profile.json (mode 0600). Policy and memory are
recommended, never required; Enter skips any line. Step 2 picks from the
provider catalog (a number or a name — `kimi`, `claude`, `ollama`, …)
with the endpoint and model prefilled and validated; a short live probe
runs after saving (skipped when stdin is piped). Answers are stored
locally and never printed back.
Piped stdin answers one line per prompt, in order: workspace, provider,
url, model, key, policy url, policy key, console url, memory url, memory key.
Steps 3 and 4 are optional: their URL prompts validate, Enter skips, and
a command-shaped answer (e.g. `engram pair`) gets the run-it-in-another-
window hint instead of being saved.

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
    use amparo_inference::{ChatRequest, InferenceError, InferenceResponse, InferenceStream};

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
        // Ten answers: workspace, provider, url, model, key, policy url,
        // policy key, console url, memory url, memory key.
        let mut stdin: &[u8] = b"/tmp/unused-workspace\nopenai\nhttp://127.0.0.1:11434/v1\nqwen\n\nhttp://127.0.0.1:8080\n\nhttp://127.0.0.1:9101\n\n\n";
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
        assert_eq!(
            profile.inference_provider.as_deref(),
            Some("openai"),
            "the profile stores the catalog id"
        );
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
    fn run_short_spawns_onto_an_ambient_runtime() {
        // The regression for "Cannot start a runtime from within a
        // runtime": inside an ambient tokio runtime the future must be
        // spawned onto it, never driven by a nested runtime.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let answer = runtime.block_on(async {
            run_short("check", Duration::from_secs(5), async { 42u32 }).unwrap()
        });
        assert_eq!(answer, 42);
    }

    #[test]
    fn run_short_times_out_instead_of_hanging() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let err = runtime.block_on(async {
            run_short(
                "check",
                Duration::from_millis(100),
                async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    42u32
                },
            )
            .unwrap_err()
        });
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn run_short_without_a_runtime_uses_a_throwaway_one() {
        // A plain sync thread (no ambient runtime) still completes — the
        // bare-`amparo` boot path if it ever runs outside the runtime.
        let answer = run_short("check", Duration::from_secs(5), async { 7u32 }).unwrap();
        assert_eq!(answer, 7);
    }

    #[test]
    fn optional_url_prompts_skip_validate_and_repair() {
        // Enter skips — an optional step, never an error.
        let mut reader: &[u8] = b"\n";
        assert_eq!(ask_optional_url(&mut reader, None).unwrap(), "");
        // A command-shaped answer is caught and re-prompted, then a valid
        // URL lands (the exact `engram pair` trap from the first drill).
        let mut reader: &[u8] = b"engram pair\nhttp://127.0.0.1:8787\n";
        assert_eq!(
            ask_optional_url(&mut reader, None).unwrap(),
            "http://127.0.0.1:8787"
        );
        // A valid default is offered on Enter (the caller's pick keeps
        // it), and a pasted suffix is normalized away.
        let mut reader: &[u8] = b"\n";
        assert_eq!(
            ask_optional_url(&mut reader, Some("http://localhost:8080".into())).unwrap(),
            ""
        );
        let mut reader: &[u8] = b"https://console.example/path/chat/completions\n";
        assert_eq!(
            ask_optional_url(&mut reader, None).unwrap(),
            "https://console.example/path"
        );
        // A saved junk value is dropped on Enter instead of echoed back.
        let mut reader: &[u8] = b"\n";
        assert_eq!(
            ask_optional_url(&mut reader, Some("engram pair".into())).unwrap(),
            ""
        );
    }

    /// Run the picker over the given lines; the default spec is openai.
    fn picker(lines: &str) -> &'static catalog::ProviderSpec {
        let mut reader: &[u8] = lines.as_bytes();
        pick_provider(&mut reader, catalog::lookup("openai").unwrap()).unwrap()
    }

    #[test]
    fn picker_accepts_number_name_alias_and_default() {
        assert_eq!(picker("3\n").id, "moonshot");
        assert_eq!(picker("kimi\n").id, "moonshot", "aliases resolve");
        assert_eq!(picker("MOONSHOT\n").id, "moonshot", "case-insensitive");
        assert_eq!(picker("\n").id, "openai", "Enter keeps the default");
        assert_eq!(picker("13\n").id, "custom", "the last number works");
        assert_eq!(
            picker("banana\n2\n").id,
            "openai",
            "an unknown answer re-prompts"
        );
        assert_eq!(picker("14\n7\n").id, "mistral", "out of range re-prompts");
    }

    #[test]
    fn url_prompt_validates_and_repairs() {
        // Enter keeps a valid default.
        let mut reader: &[u8] = b"\n";
        assert_eq!(
            ask_url(&mut reader, Some("http://localhost:11434".into())).unwrap(),
            "http://localhost:11434"
        );
        // A pasted /chat/completions suffix is normalized away.
        let mut reader: &[u8] = b"https://api.moonshot.cn/v1/chat/completions\n";
        assert_eq!(
            ask_url(&mut reader, None).unwrap(),
            "https://api.moonshot.cn/v1"
        );
        // Junk re-prompts, then a valid answer lands.
        let mut reader: &[u8] = b"not a url\n  http://192.168.1.7:8080  \n";
        assert_eq!(ask_url(&mut reader, None).unwrap(), "http://192.168.1.7:8080");
        // No default: Enter re-prompts instead of saving nothing.
        let mut reader: &[u8] = b"\nhttps://x.example\n";
        assert_eq!(ask_url(&mut reader, None).unwrap(), "https://x.example");
        // A saved junk value is not offered as the default.
        let mut reader: &[u8] = b"\nhttps://x.example\n";
        assert_eq!(
            ask_url(&mut reader, Some("junk value".into())).unwrap(),
            "https://x.example"
        );
    }

    #[test]
    fn model_prompt_keeps_default_or_requires_one() {
        let mut reader: &[u8] = b"\n";
        assert_eq!(
            ask_model(&mut reader, Some("gpt-4o".into())).unwrap(),
            "gpt-4o"
        );
        let mut reader: &[u8] = b"\nllama3.1:8b\n";
        assert_eq!(ask_model(&mut reader, None).unwrap(), "llama3.1:8b");
    }

    #[test]
    fn infer_desc_names_the_catalog_label() {
        let mut profile = full();
        profile.inference_provider = Some("moonshot".into());
        profile.inference_model = Some("kimi-k3".into());
        let line = infer_desc(&profile);
        assert!(
            line.starts_with("Moonshot AI (Kimi) · kimi-k3"),
            "{line}"
        );
        // Unknown ids fall back to the raw string, never panic.
        profile.inference_provider = Some("banana".into());
        let line = infer_desc(&profile);
        assert!(line.starts_with("banana · "), "{line}");
    }

    #[test]
    fn probe_text_is_one_clean_line() {
        assert_eq!(probe_text("ok"), "ok");
        assert_eq!(probe_text("bad\nline"), "badline");
        assert_eq!(probe_text("  ok  "), "ok");
    }

    struct ProbeOk;

    #[async_trait::async_trait]
    impl InferenceProvider for ProbeOk {
        async fn complete(
            &self,
            request: InferenceRequest,
        ) -> Result<InferenceResponse, InferenceError> {
            assert!(
                request.prompt.contains("ok"),
                "the probe prompt must be cheap: {}",
                request.prompt
            );
            Ok(InferenceResponse {
                text: "ok".into(),
                tokens: 1,
                finish_reason: "stop".into(),
            })
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
            Err(InferenceError::Config("no embeddings".into()))
        }

        async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            Ok(vec![])
        }

        fn default_model(&self) -> String {
            "probe-model".into()
        }

        async fn complete_chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<InferenceStream, InferenceError> {
            Err(InferenceError::Config("no stream".into()))
        }
    }

    struct ProbeFail;

    #[async_trait::async_trait]
    impl InferenceProvider for ProbeFail {
        async fn complete(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, InferenceError> {
            Err(InferenceError::Config("boom".into()))
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
            Err(InferenceError::Config("no embeddings".into()))
        }

        async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            Ok(vec![])
        }

        fn default_model(&self) -> String {
            "probe-model".into()
        }

        async fn complete_chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<InferenceStream, InferenceError> {
            Err(InferenceError::Config("no stream".into()))
        }
    }

    #[tokio::test]
    async fn probe_reports_the_answer_or_the_failure() {
        let answer = probe_provider(&ProbeOk, "kimi-k3").await.unwrap();
        assert_eq!(answer, "ok");
        let err = probe_provider(&ProbeFail, "kimi-k3").await.unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }
}
