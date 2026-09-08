//! Provider catalog — the single source of truth for the providers Amparo
//! knows how to talk to.
//!
//! Every entry names the wire protocol the provider speaks, its default
//! endpoint and model, and any behavioral flags (notably whether assistant
//! turns must echo `reasoning_content` back verbatim for multi-turn tool
//! calling to work — required by Moonshot/Kimi K3, DeepSeek, and the other
//! always-on reasoning endpoints).
//!
//! `AMPARO_INFERENCE_PROVIDER` and the setup wizard both resolve through
//! [`lookup`], so the catalog id (or any alias) is what users ever type.

use crate::{InferenceError, ProviderKind};

/// A cataloged provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    /// Catalog id — the value `AMPARO_INFERENCE_PROVIDER` accepts.
    pub id: &'static str,
    /// Human-friendly label for the wizard picker and the `[infer]` line.
    pub label: &'static str,
    /// Wire protocol this provider speaks.
    pub wire: ProviderKind,
    /// Default base URL; `None` means the user must supply one.
    pub base_url: Option<&'static str>,
    /// Default model id; `None` means the user must supply one.
    pub default_model: Option<&'static str>,
    /// Assistant turns carry a `reasoning_content` field that must be
    /// echoed back verbatim on the next request, or tool calling breaks.
    pub reasoning: bool,
    /// Alternate ids that resolve to this spec (e.g. `kimi` → `moonshot`).
    pub aliases: &'static [&'static str],
    /// Whether an API key is expected (`false` for keyless local servers).
    pub key_expected: bool,
}

/// The catalog. Defaults are prefilled, not authoritative — every field is
/// editable in the wizard, and `AMPARO_INFERENCE_URL`/`_MODEL` override them.
pub const CATALOG: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        label: "Anthropic (Claude)",
        wire: ProviderKind::Anthropic,
        base_url: Some("https://api.anthropic.com"),
        default_model: Some("claude-sonnet-5-20250929"),
        reasoning: false,
        aliases: &["claude"],
        key_expected: true,
    },
    ProviderSpec {
        id: "openai",
        label: "OpenAI (GPT)",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://api.openai.com/v1"),
        default_model: Some("gpt-4o"),
        reasoning: false,
        aliases: &["gpt"],
        key_expected: true,
    },
    ProviderSpec {
        id: "moonshot",
        label: "Moonshot AI (Kimi)",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://api.moonshot.cn/v1"),
        default_model: Some("kimi-k3"),
        reasoning: true,
        aliases: &["kimi", "kimi-k3", "kimi-k2", "k2"],
        key_expected: true,
    },
    ProviderSpec {
        id: "deepseek",
        label: "DeepSeek",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://api.deepseek.com/v1"),
        default_model: Some("deepseek-chat"),
        reasoning: true,
        aliases: &[],
        key_expected: true,
    },
    ProviderSpec {
        id: "qwen",
        label: "Qwen (Alibaba)",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        default_model: Some("qwen-plus"),
        reasoning: true,
        aliases: &["dashscope"],
        key_expected: true,
    },
    ProviderSpec {
        id: "glm",
        label: "GLM (Zhipu)",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://open.bigmodel.cn/api/paas/v4"),
        default_model: Some("glm-4.5"),
        reasoning: true,
        aliases: &["zhipu"],
        key_expected: true,
    },
    ProviderSpec {
        id: "mistral",
        label: "Mistral",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://api.mistral.ai/v1"),
        default_model: Some("mistral-large-latest"),
        reasoning: false,
        aliases: &[],
        key_expected: true,
    },
    ProviderSpec {
        id: "groq",
        label: "Groq",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://api.groq.com/openai/v1"),
        default_model: Some("llama-3.3-70b-versatile"),
        reasoning: false,
        aliases: &[],
        key_expected: true,
    },
    ProviderSpec {
        id: "openrouter",
        label: "OpenRouter",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://openrouter.ai/api/v1"),
        default_model: None,
        reasoning: false,
        aliases: &[],
        key_expected: true,
    },
    ProviderSpec {
        id: "gemini",
        label: "Google (Gemini)",
        wire: ProviderKind::OpenAI,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        default_model: Some("gemini-2.5-flash"),
        reasoning: false,
        aliases: &["google"],
        key_expected: true,
    },
    ProviderSpec {
        id: "ollama",
        label: "Ollama (local)",
        wire: ProviderKind::OllamaNative,
        base_url: Some("http://localhost:11434"),
        default_model: None,
        reasoning: false,
        aliases: &[],
        key_expected: false,
    },
    ProviderSpec {
        id: "lmstudio",
        label: "LM Studio (local)",
        wire: ProviderKind::OpenAI,
        base_url: Some("http://localhost:1234/v1"),
        default_model: None,
        reasoning: false,
        aliases: &[],
        key_expected: false,
    },
    ProviderSpec {
        id: "custom",
        label: "Other (OpenAI-compatible)",
        wire: ProviderKind::OpenAI,
        base_url: None,
        default_model: None,
        reasoning: false,
        aliases: &["other", "openai-compatible"],
        key_expected: true,
    },
];

/// Resolve a provider by catalog id or alias, case-insensitively.
pub fn lookup(input: &str) -> Option<&'static ProviderSpec> {
    let needle = input.trim().to_lowercase();
    CATALOG.iter().find(|spec| {
        spec.id.eq_ignore_ascii_case(&needle)
            || spec.aliases.iter().any(|a| a.eq_ignore_ascii_case(&needle))
    })
}

/// The catalog ids (not aliases), for error messages and the wizard.
pub fn known_ids() -> Vec<&'static str> {
    CATALOG.iter().map(|spec| spec.id).collect()
}

/// Normalize a user-typed base URL:
/// - trim surrounding whitespace and trailing `/`;
/// - strip a pasted trailing `/chat/completions`;
/// - require an `http://`/`https://` scheme and a non-empty host.
///
/// Returns the normalized URL or a descriptive configuration error. The
/// wizard calls this before saving; `InferenceConfig::build()` calls it
/// before constructing a provider, so a junk URL fails at configuration
/// time instead of the `(unparseable url)` class of runtime failure.
pub fn normalize_base_url(input: &str) -> Result<String, InferenceError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(InferenceError::Config(
            "inference URL must not be empty — e.g. https://api.moonshot.cn/v1"
                .to_string(),
        ));
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(InferenceError::Config(format!(
            "invalid inference URL '{raw}' — spaces are not allowed"
        )));
    }
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        InferenceError::Config(format!(
            "invalid inference URL '{raw}' — expected http:// or https:// followed by a host"
        ))
    })?;
    if !matches!(scheme, "http" | "https") {
        return Err(InferenceError::Config(format!(
            "invalid inference URL scheme '{scheme}://' — expected http:// or https://"
        )));
    }
    if rest.is_empty() || rest.starts_with('/') {
        return Err(InferenceError::Config(format!(
            "invalid inference URL '{raw}' — missing host"
        )));
    }
    let mut path = rest.trim_end_matches('/').to_string();
    if path.ends_with("/chat/completions") {
        path.truncate(path.len() - "/chat/completions".len());
        path = path.trim_end_matches('/').to_string();
    }
    // host[:port] is everything up to the first '/'.
    if path.split('/').next().unwrap_or("").is_empty() {
        return Err(InferenceError::Config(format!(
            "invalid inference URL '{raw}' — missing host"
        )));
    }
    Ok(format!("{scheme}://{path}"))
}

/// Append `/v1` when the URL carries no path, for OpenAI-compatible
/// endpoints (`https://api.openai.com` → `https://api.openai.com/v1`).
/// URLs that already have a path (including `/v1`) pass through unchanged.
pub fn append_v1_if_bare(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    if rest.contains('/') {
        url.to_string()
    } else {
        format!("{url}/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive_and_alias_aware() {
        assert_eq!(lookup("MOONSHOT").unwrap().id, "moonshot");
        assert_eq!(lookup("Kimi").unwrap().id, "moonshot");
        assert_eq!(lookup("kimi-k3").unwrap().id, "moonshot");
        assert_eq!(lookup("claude").unwrap().id, "anthropic");
        assert_eq!(lookup("google").unwrap().id, "gemini");
        assert_eq!(lookup("other").unwrap().id, "custom");
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn lookup_reports_wire_and_flags() {
        let kimi = lookup("kimi").unwrap();
        assert_eq!(kimi.wire, ProviderKind::OpenAI);
        assert!(kimi.reasoning);
        assert_eq!(kimi.default_model, Some("kimi-k3"));
        assert_eq!(lookup("anthropic").unwrap().wire, ProviderKind::Anthropic);
        assert_eq!(lookup("ollama").unwrap().wire, ProviderKind::OllamaNative);
        assert!(!lookup("ollama").unwrap().key_expected);
        assert!(!lookup("lmstudio").unwrap().key_expected);
        assert!(lookup("openrouter").unwrap().default_model.is_none());
        assert!(lookup("custom").unwrap().base_url.is_none());
    }

    #[test]
    fn known_ids_lists_every_catalog_entry() {
        assert!(known_ids().contains(&"moonshot"));
        assert_eq!(known_ids().len(), CATALOG.len());
    }

    #[test]
    fn normalize_base_url_matrix() {
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_base_url("  http://localhost:11434  ").unwrap(),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_base_url("https://api.moonshot.cn/v1/chat/completions").unwrap(),
            "https://api.moonshot.cn/v1"
        );
        assert_eq!(
            normalize_base_url("http://192.168.1.7:8080").unwrap(),
            "http://192.168.1.7:8080"
        );
        assert_eq!(
            normalize_base_url("https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai/api/v1"
        );
    }

    #[test]
    fn normalize_base_url_rejects_junk() {
        for bad in [
            "",
            "not a url",
            "localhost:11434",
            "ftp://example.com",
            "https://",
            "https:///missing-host",
            "http://host with spaces:1234",
        ] {
            let err = normalize_base_url(bad).unwrap_err();
            assert!(matches!(err, InferenceError::Config(_)), "{bad:?}: {err}");
        }
    }

    #[test]
    fn append_v1_only_when_pathless() {
        assert_eq!(append_v1_if_bare("https://api.openai.com"), "https://api.openai.com/v1");
        assert_eq!(append_v1_if_bare("https://api.openai.com/v1"), "https://api.openai.com/v1");
        assert_eq!(append_v1_if_bare("https://host/api"), "https://host/api");
        assert_eq!(
            append_v1_if_bare("http://localhost:11434"),
            "http://localhost:11434/v1"
        );
    }
}
