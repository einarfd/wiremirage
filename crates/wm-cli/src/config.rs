//! Profile-based configuration for `wm`.
//!
//! A profile is a named (host, token) pair living in
//! `~/.config/wiremirage/config.toml` (override via `WM_CONFIG_FILE`).
//! The selected profile fills in host and token for any call that
//! doesn't have them set via flag or environment.
//!
//! Resolution order (per [[cli-design.md]]):
//!   1. `--host` / `--token` flags
//!   2. `WM_HOST` / `WM_TOKEN` environment
//!   3. Selected profile's `host` / `token`
//!   4. `http://localhost:8080` for host; auth-required commands fail
//!      without a token.
//!
//! Profile selection: `--profile` > `WM_PROFILE` > `default`. A missing
//! `default` profile is silent — the CLI works exactly as before for
//! users who haven't set up a config file.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// Final fallback when nothing else specifies a host. Matches the
/// docker-compose dev setup the README walks operators through.
pub const DEFAULT_HOST: &str = "http://localhost:8080";

/// Parsed configuration. Wraps the raw file shape with a single
/// lookup helper. The empty `Config` (no file, no env override) is
/// the common case for users who only ever talk to one host.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

/// One named (host, token) pair. Both fields are optional so that a
/// profile can override only what it needs — e.g. a `[profiles.prod]`
/// that just sets `host` and inherits the token from `WM_TOKEN`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Profile {
    pub host: Option<String>,
    pub token: Option<String>,
}

/// The values dispatch actually uses, once flags / env / profile have
/// been combined. `host` is always present (default applies if
/// nothing else); `token` is `None` if no source provided one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub host: String,
    pub token: Option<String>,
}

impl Config {
    /// Load the config file from `WM_CONFIG_FILE`, falling back to
    /// `$XDG_CONFIG_HOME/wiremirage/config.toml`, then
    /// `$HOME/.config/wiremirage/config.toml`.
    ///
    /// Missing file → empty `Config` (not an error). Malformed TOML
    /// → `Err` with the file path so the user can find what to fix.
    pub fn load() -> Result<Self> {
        let Some(path) = Self::resolve_path() else {
            // No HOME and no override — common in some sandboxed test
            // environments. Behaviour is "no config file"; the CLI
            // works exactly as it did before profiles existed.
            return Ok(Self::default());
        };
        Self::load_from_path(&path)
    }

    fn load_from_path(path: &std::path::Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parse config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow!("read config file {}: {e}", path.display())),
        }
    }

    fn resolve_path() -> Option<PathBuf> {
        if let Ok(custom) = env::var("WM_CONFIG_FILE")
            && !custom.is_empty()
        {
            return Some(PathBuf::from(custom));
        }
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("wiremirage").join("config.toml"));
        }
        if let Ok(home) = env::var("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("wiremirage")
                    .join("config.toml"),
            );
        }
        None
    }

    /// Look up a profile by name. Returns `None` if the profile
    /// doesn't exist; callers distinguish "profile was explicitly
    /// named but missing" (an error) from "no profile selected and
    /// `default` happens not to be configured" (fine).
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Apply profile values to flag/env values to produce the
    /// dispatch-ready `Effective` config. `profile_arg` is the value
    /// of `--profile` or `WM_PROFILE` (already merged by clap); when
    /// it's `Some` and the profile doesn't exist, return an error so
    /// the user notices the typo. When it's `None`, fall back to
    /// `default` and tolerate it not existing.
    pub fn resolve(
        &self,
        profile_arg: Option<&str>,
        flag_host: Option<String>,
        flag_token: Option<String>,
    ) -> Result<Effective> {
        let (profile_name, was_explicit) = match profile_arg {
            Some(name) => (name, true),
            None => ("default", false),
        };

        let profile = match (self.profile(profile_name), was_explicit) {
            (Some(p), _) => Some(p),
            (None, true) => {
                let available: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                let suffix = if available.is_empty() {
                    "no profiles are configured".to_string()
                } else {
                    format!("available: {}", available.join(", "))
                };
                return Err(anyhow!(
                    "profile {profile_name:?} not found in config file ({suffix})"
                ));
            }
            (None, false) => None,
        };

        let host = flag_host
            .or_else(|| profile.and_then(|p| p.host.clone()))
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        let token = flag_token.or_else(|| profile.and_then(|p| p.token.clone()));

        Ok(Effective { host, token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("parse")
    }

    #[test]
    fn empty_config_falls_back_to_default_host_with_no_token() {
        let cfg = Config::default();
        let eff = cfg.resolve(None, None, None).unwrap();
        assert_eq!(eff.host, DEFAULT_HOST);
        assert_eq!(eff.token, None);
    }

    #[test]
    fn default_profile_supplies_host_and_token() {
        let cfg = config_with(
            r#"
            [profiles.default]
            host = "https://wm.example.com"
            token = "wmt_default"
            "#,
        );
        let eff = cfg.resolve(None, None, None).unwrap();
        assert_eq!(eff.host, "https://wm.example.com");
        assert_eq!(eff.token.as_deref(), Some("wmt_default"));
    }

    #[test]
    fn flag_host_wins_over_profile() {
        let cfg = config_with(
            r#"
            [profiles.default]
            host = "https://from-profile"
            "#,
        );
        let eff = cfg
            .resolve(None, Some("https://from-flag".into()), None)
            .unwrap();
        assert_eq!(eff.host, "https://from-flag");
    }

    #[test]
    fn flag_token_wins_over_profile() {
        let cfg = config_with(
            r#"
            [profiles.default]
            token = "wmt_from_profile"
            "#,
        );
        let eff = cfg
            .resolve(None, None, Some("wmt_from_flag".into()))
            .unwrap();
        assert_eq!(eff.token.as_deref(), Some("wmt_from_flag"));
    }

    #[test]
    fn named_profile_selection_works() {
        let cfg = config_with(
            r#"
            [profiles.default]
            host = "http://localhost:8080"
            [profiles.prod]
            host = "https://wm.prod.example"
            token = "wmt_prod"
            "#,
        );
        let eff = cfg.resolve(Some("prod"), None, None).unwrap();
        assert_eq!(eff.host, "https://wm.prod.example");
        assert_eq!(eff.token.as_deref(), Some("wmt_prod"));
    }

    #[test]
    fn explicit_missing_profile_errors_with_available_list() {
        let cfg = config_with(
            r#"
            [profiles.default]
            host = "http://x"
            [profiles.prod]
            host = "http://y"
            "#,
        );
        let err = cfg.resolve(Some("staging"), None, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("staging"), "names the bad profile: {msg}");
        assert!(
            msg.contains("default") && msg.contains("prod"),
            "lists available profiles: {msg}"
        );
    }

    #[test]
    fn missing_default_is_silent_when_no_profile_requested() {
        // Common case: user has a config file with [profiles.prod]
        // only, runs `wm` without --profile. Resolution falls through
        // to the built-in default host without erroring.
        let cfg = config_with(
            r#"
            [profiles.prod]
            host = "https://wm.prod.example"
            token = "wmt_prod"
            "#,
        );
        let eff = cfg.resolve(None, None, None).unwrap();
        assert_eq!(eff.host, DEFAULT_HOST);
        assert_eq!(eff.token, None);
    }

    #[test]
    fn partial_profile_fills_only_what_it_has() {
        // [profiles.default] sets host but not token. WM_TOKEN /
        // --token still has to come from outside; we don't invent.
        let cfg = config_with(
            r#"
            [profiles.default]
            host = "https://only-host"
            "#,
        );
        let eff = cfg.resolve(None, None, None).unwrap();
        assert_eq!(eff.host, "https://only-host");
        assert_eq!(eff.token, None);
    }

    #[test]
    fn missing_file_returns_empty_config() {
        // A path we know doesn't exist exercises the NotFound branch
        // without needing tempfile machinery — the function just maps
        // io::ErrorKind::NotFound to an empty Config.
        let path = std::env::temp_dir().join(format!("wm-cli-missing-{}.toml", std::process::id()));
        // Make sure it really isn't there — process-id-suffixed path
        // is enough on any reasonable filesystem.
        let _ = fs::remove_file(&path);
        let cfg = Config::load_from_path(&path).unwrap();
        assert!(cfg.profiles.is_empty());
    }
}
