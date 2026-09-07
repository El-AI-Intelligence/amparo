//! `amparo update` — compare the running binary against the published site.
//!
//! Two consumers:
//!
//! - `amparo update check [--url URL]` — the explicit operator command
//!   (exit 0 = check completed, 1 = check failed, 2 = usage).
//! - the TUI boot banner — [`check_and_cache`] runs once per TUI process
//!   (5-second cap, fail-silent: an update notice must never delay or
//!   break the prompt) and [`cached_note`] feeds the `[update]` line.
//!
//! The site endpoint (`https://amparo.ellmstack.dev/version.json`) is a
//! static file generated at deploy time; the payload must carry
//! `product: "amparo"` and a semver `version` before it is trusted.

use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;

pub const DEFAULT_UPDATE_URL: &str = "https://amparo.ellmstack.dev/version.json";
pub const INSTALL_SCRIPT_URL: &str = "https://downloads.ellmstack.dev/amparo/install.sh";
pub const CHANGELOG_URL: &str = "https://amparo.ellmstack.dev/changelog";

/// The `/version.json` payload — the fields update checks actually use.
/// Unknown fields are ignored so the endpoint can grow without breaking
/// older binaries.
#[derive(Deserialize)]
struct VersionInfo {
    product: String,
    version: String,
    #[serde(default)]
    changelog_url: Option<String>,
    #[serde(default)]
    install_url: Option<String>,
}

/// The TUI banner's cached notice — only set when the site is strictly
/// newer than this binary.
#[derive(Clone, Debug)]
pub struct UpdateNote {
    pub latest: String,
}

static NOTE: OnceLock<Option<UpdateNote>> = OnceLock::new();

const UPDATE_USAGE: &str = "\
amparo update — compare the running binary against the published site

USAGE:
  amparo update check [--url URL]

FLAGS:
  --url URL    version.json endpoint (default: https://amparo.ellmstack.dev/version.json)

EXIT CODES:
  0  check completed (latest, or an update is available)
  1  check failed (network, HTTP, or payload rejected)
  2  usage error";

enum ParsedUpdate {
    Help,
    Error(String),
    Check { url: String },
}

fn parse(args: Vec<String>) -> ParsedUpdate {
    let mut it = args.into_iter();
    let Some(sub) = it.next() else {
        return ParsedUpdate::Error("missing subcommand; see `amparo update --help`".into());
    };
    match sub.as_str() {
        "check" => {}
        "--help" | "-h" => return ParsedUpdate::Help,
        other => return ParsedUpdate::Error(format!("unknown subcommand {other}; see `amparo update --help`")),
    }
    let mut url = DEFAULT_UPDATE_URL.to_string();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--url" => {
                let Some(value) = it.next() else {
                    return ParsedUpdate::Error("--url requires a value".into());
                };
                if !value.starts_with("http://") && !value.starts_with("https://") {
                    return ParsedUpdate::Error("--url must be http(s)".into());
                }
                url = value;
            }
            other => return ParsedUpdate::Error(format!("unknown flag {other}; see `amparo update --help`")),
        }
    }
    ParsedUpdate::Check { url }
}

pub async fn dispatch(args: impl Iterator<Item = String>) {
    match parse(args.collect()) {
        ParsedUpdate::Help => println!("{UPDATE_USAGE}"),
        ParsedUpdate::Error(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        ParsedUpdate::Check { url } => {
            if let Err(message) = check_update(&url, env!("CARGO_PKG_VERSION")).await {
                eprintln!("amparo update: {message}");
                std::process::exit(1);
            }
        }
    }
}

/// Split a semver core (`1.2.3`, optional `-pre`/`+build`) into a numeric
/// triple. `None` when the shape is not a semver at all.
pub fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split('-').next().unwrap_or(v).split('+').next().unwrap_or(v);
    let mut parts = core.split('.');
    let a: u64 = parts.next()?.parse().ok()?;
    let b: u64 = parts.next()?.parse().ok()?;
    let c: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// Strict numeric comparison — `latest > current`. `None` when either
/// side is not a semver (the caller then fails the check rather than
/// guessing).
pub fn version_is_newer(latest: &str, current: &str) -> Option<bool> {
    let l = version_triple(latest)?;
    let c = version_triple(current)?;
    Some(l > c)
}

/// The payload must actually be Amparo's before it is trusted: a
/// misconfigured `--url` pointing at another product's version.json
/// must not drive an update notice.
fn validate_version_info(info: &VersionInfo) -> Result<(), String> {
    if info.product != "amparo" {
        return Err(format!(
            "endpoint advertises product {:?}, expected \"amparo\"",
            info.product
        ));
    }
    if version_triple(&info.version).is_none() {
        return Err(format!("endpoint version {:?} is not a semver", info.version));
    }
    Ok(())
}

/// One update check against `url`, comparing the published version with
/// `current`. Prints the verdict on stdout (the `update` command's final
/// answer, per the scripting contract) and returns `Err` on any failure.
pub async fn check_update(url: &str, current: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("client setup failed: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("check failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update server returned HTTP {}", resp.status()));
    }
    let info: VersionInfo = resp
        .json()
        .await
        .map_err(|e| format!("update server response was not valid JSON: {e}"))?;
    validate_version_info(&info)?;

    match version_is_newer(&info.version, current) {
        Some(true) => {
            println!("Update available: amparo {} (you have {}).", info.version, current);
            println!("Upgrade: curl -fsSL {} | bash", info.install_url.as_deref().unwrap_or(INSTALL_SCRIPT_URL));
            println!(
                "Changelog: {}",
                info.changelog_url.as_deref().unwrap_or(CHANGELOG_URL)
            );
        }
        _ => println!("amparo {current} is the latest version ({} on the site).", info.version),
    }
    Ok(())
}

/// The TUI's cached notice, if the boot check found a newer version.
pub fn cached_note() -> Option<&'static UpdateNote> {
    NOTE.get().and_then(Option::as_ref)
}

/// Run once per TUI boot: check the default endpoint, cache the notice
/// when the site is newer. Fail-silent — any error (offline, timeout,
/// rejected payload) just means no banner line.
pub async fn check_and_cache() {
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(DEFAULT_UPDATE_URL).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let info: VersionInfo = resp.json().await.ok()?;
        validate_version_info(&info).ok()?;
        version_is_newer(&info.version, env!("CARGO_PKG_VERSION"))
            .unwrap_or(false)
            .then_some(info.version)
    })
    .await;
    let latest = match outcome {
        Ok(Some(latest)) => latest,
        _ => return,
    };
    let _ = NOTE.set(Some(UpdateNote { latest }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_parses_core_and_prerelease() {
        assert_eq!(version_triple("0.12.0"), Some((0, 12, 0)));
        assert_eq!(version_triple("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(version_triple("2.0.0+build7"), Some((2, 0, 0)));
        assert_eq!(version_triple("0.12"), None);
        assert_eq!(version_triple("0.12.0.1"), None);
        assert_eq!(version_triple("latest"), None);
        assert_eq!(version_triple(""), None);
    }

    #[test]
    fn newer_is_strict_numeric_compare() {
        assert_eq!(version_is_newer("0.13.0", "0.12.0"), Some(true));
        assert_eq!(version_is_newer("0.12.0", "0.13.0"), Some(false));
        assert_eq!(version_is_newer("0.12.1", "0.12.0"), Some(true));
        assert_eq!(version_is_newer("1.0.0", "0.12.0"), Some(true));
        assert_eq!(version_is_newer("0.12.0", "0.12.0"), Some(false));
        // Prereleases are not newer than their own release.
        assert_eq!(version_is_newer("0.13.0-rc.1", "0.12.0"), Some(true));
        assert_eq!(version_is_newer("not-semver", "0.12.0"), None);
    }

    #[test]
    fn payload_shape_is_validated() {
        let ok = VersionInfo {
            product: "amparo".into(),
            version: "0.13.0".into(),
            changelog_url: Some("https://amparo.ellmstack.dev/changelog".into()),
            install_url: None,
        };
        assert!(validate_version_info(&ok).is_ok());

        let wrong_product = VersionInfo {
            product: "guardrail".into(),
            version: "0.13.0".into(),
            changelog_url: None,
            install_url: None,
        };
        assert!(validate_version_info(&wrong_product).is_err());

        let bad_version = VersionInfo {
            product: "amparo".into(),
            version: "soon".into(),
            changelog_url: None,
            install_url: None,
        };
        assert!(validate_version_info(&bad_version).is_err());
    }
}
