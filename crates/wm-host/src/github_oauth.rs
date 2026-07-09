//! GitHub OAuth user-login flow.
//!
//! GitHub uses OAuth 2.0 (not strict OIDC), so we exchange the
//! authorization code for an access token and then call three GitHub
//! REST endpoints ourselves to assemble an identity:
//!
//! * `GET /user` — numeric `id` (our stable subject), login, name,
//!   and email (often null if private).
//! * `GET /user/emails` — list of `{email, primary, verified}`; the
//!   primary verified entry is what we attach to the user record.
//! * `GET /user/orgs` — list of `{login, id}`; consulted only when
//!   the operator configured an org-membership allow-list.
//!
//! Configuration is per [[adrs/0010-oauth-oidc.md]]'s GitHub-at-v1
//! narrowing. The operator registers a GitHub OAuth app, drops the
//! credentials in `WM_GITHUB_CLIENT_ID` / `WM_GITHUB_CLIENT_SECRET`,
//! and configures at least one allow rule via `WM_GITHUB_ALLOW_USERS`
//! and/or `WM_GITHUB_ALLOW_ORGS`. Without an allow rule, no GitHub
//! account can log in — by design.

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// GitHub-side endpoints. Overridable for testing so the e2e test
/// harness can point at an in-process mock — production code never
/// touches the fields directly, just the convenience constructors.
#[derive(Debug, Clone)]
pub struct GitHubEndpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub api_base_url: String,
}

impl Default for GitHubEndpoints {
    fn default() -> Self {
        Self {
            authorize_url: "https://github.com/login/oauth/authorize".to_string(),
            token_url: "https://github.com/login/oauth/access_token".to_string(),
            api_base_url: "https://api.github.com".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Explicit GitHub usernames (logins) allowed to log in. OR'd
    /// against `allow_orgs` — a user passes if any rule matches.
    pub allow_users: Vec<String>,
    /// GitHub org logins. A user passes if they're a (visible)
    /// member of any listed org. Requires the `read:org` scope on
    /// the OAuth token, which we always request.
    pub allow_orgs: Vec<String>,
    /// GitHub logins promoted to `is_admin = true` on first login.
    /// Optional — when empty, every GitHub user lands as a non-admin
    /// and the bootstrap token's user can promote them via
    /// `wm users update <login> --admin`.
    pub admin_users: Vec<String>,
    pub endpoints: GitHubEndpoints,
}

impl GitHubConfig {
    /// Parse `WM_GITHUB_CLIENT_ID` / `_SECRET` / `_ALLOW_USERS` /
    /// `_ALLOW_ORGS` from env. Returns `Ok(None)` when no GitHub
    /// credentials are set (operator hasn't opted into the flow);
    /// `Err` when partially configured so a typo surfaces fast.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_env_values(EnvValues {
            client_id: env::var("WM_GITHUB_CLIENT_ID").ok(),
            client_secret: env::var("WM_GITHUB_CLIENT_SECRET").ok(),
            allow_users: env::var("WM_GITHUB_ALLOW_USERS").unwrap_or_default(),
            allow_orgs: env::var("WM_GITHUB_ALLOW_ORGS").unwrap_or_default(),
            admin_users: env::var("WM_GITHUB_ADMIN_USERS").unwrap_or_default(),
        })
    }

    /// Pure-function variant of `from_env` that takes the
    /// already-resolved values. Lets unit tests cover the parsing
    /// rules without poking the process environment (which would
    /// race against other parallel tests).
    fn from_env_values(values: EnvValues) -> Result<Option<Self>> {
        let client_id = values.client_id.filter(|s| !s.is_empty());
        let client_secret = values.client_secret.filter(|s| !s.is_empty());
        match (client_id, client_secret) {
            (None, None) => Ok(None),
            (Some(_), None) | (None, Some(_)) => Err(anyhow!(
                "WM_GITHUB_CLIENT_ID and WM_GITHUB_CLIENT_SECRET must both be set, or neither"
            )),
            (Some(client_id), Some(client_secret)) => {
                let allow_users = parse_csv(&values.allow_users);
                let allow_orgs = parse_csv(&values.allow_orgs);
                let admin_users = parse_csv(&values.admin_users);
                if allow_users.is_empty() && allow_orgs.is_empty() {
                    return Err(anyhow!(
                        "GitHub login is configured but no allow rules are set. \
                         Set WM_GITHUB_ALLOW_USERS=user1,user2 and/or \
                         WM_GITHUB_ALLOW_ORGS=org1,org2; without one, no GitHub \
                         account can log in (refusing to start so this misconfiguration \
                         surfaces fast rather than silently denying every login)."
                    ));
                }
                Ok(Some(Self {
                    client_id,
                    client_secret,
                    allow_users,
                    allow_orgs,
                    admin_users,
                    endpoints: GitHubEndpoints::default(),
                }))
            }
        }
    }

    /// Build the URL we redirect the user's browser to. `state` is the
    /// CSRF nonce — the caller persists it server-side and validates
    /// the round-trip on the callback. `redirect_uri` is what GitHub
    /// posts back to; it MUST exactly match the URL registered on
    /// the GitHub OAuth app's settings page.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        // Scopes: `read:user` for the profile, `user:email` for the
        // email list, `read:org` so the org-membership endpoint
        // returns private memberships. GitHub returns scope-narrowed
        // tokens if the user declines any, which our allow-check
        // surfaces as a clear deny.
        let scope = "read:user user:email read:org";
        // `reqwest::Url` re-exports the `url` crate without forcing
        // us to take a direct dep on it.
        let mut url = reqwest::Url::parse(&self.endpoints.authorize_url)
            .expect("authorize_url is a valid URL");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("scope", scope);
        url.into()
    }

    /// Exchange the authorization code for an access token. GitHub's
    /// `access_token` endpoint accepts form-encoded bodies and
    /// returns either form-encoded (default) or JSON (`Accept:
    /// application/json`). We request JSON; easier to parse.
    pub async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<String> {
        let client = reqwest_client()?;
        // Build the urlencoded form body via reqwest::Url's
        // query_pairs encoder — we already use the `url` crate
        // transitively through reqwest for the authorize URL, and
        // this avoids taking a direct dep on `form_urlencoded` for a
        // single body.
        let mut fake = reqwest::Url::parse("https://placeholder/").expect("placeholder");
        fake.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", &self.client_secret)
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri);
        let body = fake.query().unwrap_or("").to_string();
        let resp = client
            .post(&self.endpoints.token_url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("call github token endpoint")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parse token response JSON")?;
        if !status.is_success() {
            return Err(anyhow!("github token endpoint returned {status}: {body}"));
        }
        // GitHub returns errors with 200 OK + `{"error":"bad_verification_code",...}`
        // (yes really) — we check for the field explicitly.
        if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
            let desc = body
                .get("error_description")
                .and_then(|v| v.as_str())
                .unwrap_or("(no description)");
            return Err(anyhow!("github token endpoint: {err} ({desc})"));
        }
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("github token response missing access_token: {body}"))?;
        Ok(token.to_string())
    }

    /// Fetch user identity using the access token. Fetches in
    /// parallel where it makes sense: `/user` and `/user/emails`
    /// always; `/user/orgs` only when the operator configured an
    /// org allow-list.
    pub async fn fetch_identity(&self, access_token: &str) -> Result<GitHubIdentity> {
        let client = reqwest_client()?;

        let user_url = format!("{}/user", self.endpoints.api_base_url);
        let emails_url = format!("{}/user/emails", self.endpoints.api_base_url);
        let orgs_url = format!("{}/user/orgs", self.endpoints.api_base_url);

        let need_orgs = !self.allow_orgs.is_empty();
        let user_fut = github_get_json::<UserPayload>(&client, &user_url, access_token);
        let emails_fut = github_get_json::<Vec<EmailPayload>>(&client, &emails_url, access_token);

        if need_orgs {
            let orgs_fut = github_get_json::<Vec<OrgPayload>>(&client, &orgs_url, access_token);
            let (user, emails, orgs) = tokio::try_join!(user_fut, emails_fut, orgs_fut)?;
            Ok(GitHubIdentity::assemble(user, emails, Some(orgs)))
        } else {
            let (user, emails) = tokio::try_join!(user_fut, emails_fut)?;
            Ok(GitHubIdentity::assemble(user, emails, None))
        }
    }

    /// Apply the allow rules. Returns `Ok(())` when the identity
    /// matches at least one rule; `Err` with a clear reason otherwise.
    pub fn check_allow(&self, identity: &GitHubIdentity) -> Result<(), AllowFailure> {
        // User-list: exact match on lowercased login. GitHub usernames
        // are case-insensitive on the server but case-preserving in
        // display, so a case-insensitive compare is the right thing.
        let login_lc = identity.login.to_ascii_lowercase();
        if self
            .allow_users
            .iter()
            .any(|u| u.to_ascii_lowercase() == login_lc)
        {
            return Ok(());
        }
        // Org-membership: exact match on lowercased org login.
        if let Some(orgs) = &identity.orgs {
            for member in orgs {
                let member_lc = member.to_ascii_lowercase();
                if self
                    .allow_orgs
                    .iter()
                    .any(|o| o.to_ascii_lowercase() == member_lc)
                {
                    return Ok(());
                }
            }
        }
        Err(AllowFailure {
            login: identity.login.clone(),
            checked_orgs: identity.orgs.clone().unwrap_or_default(),
        })
    }

    /// Whether this GitHub login should be promoted to admin on
    /// login. Operator opts in via `WM_GITHUB_ADMIN_USERS`; unset is
    /// "no GitHub user is admin." Case-insensitive on login.
    pub fn is_admin(&self, login: &str) -> bool {
        let lc = login.to_ascii_lowercase();
        self.admin_users
            .iter()
            .any(|u| u.to_ascii_lowercase() == lc)
    }
}

/// Pre-resolved env values fed into the pure-function `from_env_values`.
/// Kept out of `pub`-API land — only the env-reading path and the
/// tests construct one.
#[derive(Default)]
struct EnvValues {
    client_id: Option<String>,
    client_secret: Option<String>,
    allow_users: String,
    allow_orgs: String,
    admin_users: String,
}

#[derive(Debug, Clone)]
pub struct GitHubIdentity {
    /// Numeric GitHub user ID. Stable across username changes — this
    /// is the value we persist as the (provider, subject) tuple.
    pub id: u64,
    pub login: String,
    /// Primary verified email when one is available. None when the
    /// user has no verified primary on record (rare).
    pub email: Option<String>,
    /// Populated only when an org allow-list was configured. `None`
    /// signals "we didn't ask GitHub for this."
    pub orgs: Option<Vec<String>>,
}

impl GitHubIdentity {
    fn assemble(
        user: UserPayload,
        emails: Vec<EmailPayload>,
        orgs: Option<Vec<OrgPayload>>,
    ) -> Self {
        // Pick the primary verified email; fall back to the first
        // verified one; fall back to `user.email` (which may also be
        // unverified or null). Worst case: identity.email is None.
        let email = emails
            .iter()
            .find(|e| e.primary && e.verified)
            .or_else(|| emails.iter().find(|e| e.verified))
            .map(|e| e.email.clone())
            .or(user.email);
        let orgs = orgs.map(|list| list.into_iter().map(|o| o.login).collect());
        Self {
            id: user.id,
            login: user.login,
            email,
            orgs,
        }
    }
}

/// A user passed the GitHub auth step but is not on any allow-list.
#[derive(Debug, Clone)]
pub struct AllowFailure {
    pub login: String,
    /// Empty when no org allow-list was configured. Otherwise lists
    /// the orgs the user IS in, so an operator looking at logs can
    /// see whether they meant to add one of them to the allow-list.
    pub checked_orgs: Vec<String>,
}

impl std::fmt::Display for AllowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "github login {login:?} is not on this host's allow-list",
            login = self.login
        )?;
        if !self.checked_orgs.is_empty() {
            write!(f, " (user is in orgs: {})", self.checked_orgs.join(", "))?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct UserPayload {
    id: u64,
    login: String,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EmailPayload {
    email: String,
    primary: bool,
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct OrgPayload {
    login: String,
}

/// Build the reqwest client we use for every GitHub call. Short
/// timeout — these are user-blocking requests and a wedged GitHub
/// shouldn't pin a worker indefinitely. The user-agent header is
/// required by GitHub's API; using the bin name is conventional.
fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("wm-host/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")
}

async fn github_get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<T> {
    let resp = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .header("authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .with_context(|| format!("call github API: {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("github API {url} returned {status}: {body}"));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("parse github API response: {url}"))
}

fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GitHubConfig {
        GitHubConfig {
            client_id: "cid".into(),
            client_secret: "csec".into(),
            allow_users: vec!["alice".into()],
            allow_orgs: vec!["kindlyhq".into()],
            admin_users: vec!["alice".into()],
            endpoints: GitHubEndpoints::default(),
        }
    }

    fn identity_with(login: &str, orgs: Option<Vec<&str>>) -> GitHubIdentity {
        GitHubIdentity {
            id: 1,
            login: login.into(),
            email: None,
            orgs: orgs.map(|o| o.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn parse_csv_handles_whitespace_and_empties() {
        assert_eq!(parse_csv(""), Vec::<String>::new());
        assert_eq!(parse_csv(" "), Vec::<String>::new());
        assert_eq!(parse_csv("alice"), vec!["alice"]);
        assert_eq!(
            parse_csv("alice, bob ,carol "),
            vec!["alice", "bob", "carol"]
        );
        assert_eq!(parse_csv("alice,,bob"), vec!["alice", "bob"]);
    }

    #[test]
    fn authorize_url_includes_required_params() {
        let url = cfg().authorize_url("http://localhost:8080/auth/callback", "abc123");
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("redirect_uri=http"));
        assert!(url.contains("state=abc123"));
        assert!(url.contains("scope=read"));
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
    }

    #[test]
    fn allow_check_passes_on_user_match() {
        let cfg = cfg();
        let id = identity_with("alice", Some(vec!["someotherorg"]));
        assert!(cfg.check_allow(&id).is_ok());
    }

    #[test]
    fn allow_check_passes_on_org_match() {
        let cfg = cfg();
        let id = identity_with("bob", Some(vec!["kindlyhq"]));
        assert!(cfg.check_allow(&id).is_ok());
    }

    #[test]
    fn allow_check_is_case_insensitive_on_user() {
        let cfg = cfg();
        let id = identity_with("ALICE", Some(vec![]));
        assert!(cfg.check_allow(&id).is_ok());
    }

    #[test]
    fn allow_check_is_case_insensitive_on_org() {
        let cfg = cfg();
        let id = identity_with("bob", Some(vec!["KINDLYHQ"]));
        assert!(cfg.check_allow(&id).is_ok());
    }

    #[test]
    fn allow_check_denies_when_neither_user_nor_org_matches() {
        let cfg = cfg();
        let id = identity_with("eve", Some(vec!["randoorg"]));
        let err = cfg.check_allow(&id).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("eve"));
        assert!(msg.contains("randoorg"));
    }

    #[test]
    fn allow_check_denies_when_no_orgs_were_fetched() {
        // need_orgs was false → identity.orgs is None. Then only the
        // user-list matters; an unknown user is denied.
        let cfg = cfg();
        let id = identity_with("eve", None);
        let err = cfg.check_allow(&id).unwrap_err();
        assert!(err.checked_orgs.is_empty());
    }

    #[test]
    fn admin_check_is_case_insensitive() {
        let cfg = cfg();
        assert!(cfg.is_admin("alice"));
        assert!(cfg.is_admin("ALICE"));
        assert!(!cfg.is_admin("bob"));
    }

    #[test]
    fn admin_check_is_false_when_no_admins_configured() {
        let mut cfg = cfg();
        cfg.admin_users.clear();
        assert!(!cfg.is_admin("alice"));
    }

    // Pure-function tests on `from_env_values`. Don't touch the
    // process env, so parallel test execution is safe.

    #[test]
    fn from_env_returns_none_when_unconfigured() {
        assert!(
            GitHubConfig::from_env_values(EnvValues::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn from_env_rejects_partial_config() {
        let err = GitHubConfig::from_env_values(EnvValues {
            client_id: Some("x".into()),
            client_secret: None,
            ..EnvValues::default()
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("CLIENT_SECRET"));
    }

    #[test]
    fn from_env_rejects_when_no_allow_rules() {
        let err = GitHubConfig::from_env_values(EnvValues {
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_users: String::new(),
            allow_orgs: String::new(),
            ..EnvValues::default()
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("WM_GITHUB_ALLOW_USERS") || msg.contains("WM_GITHUB_ALLOW_ORGS"),
            "names the missing env vars: {msg}"
        );
    }

    #[test]
    fn from_env_accepts_either_allow_users_or_allow_orgs() {
        let just_users = GitHubConfig::from_env_values(EnvValues {
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            allow_users: "alice".into(),
            ..EnvValues::default()
        })
        .unwrap()
        .unwrap();
        assert_eq!(just_users.allow_users, vec!["alice"]);
        assert!(just_users.allow_orgs.is_empty());

        let just_orgs = GitHubConfig::from_env_values(EnvValues {
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            allow_orgs: "kindlyhq".into(),
            ..EnvValues::default()
        })
        .unwrap()
        .unwrap();
        assert!(just_orgs.allow_users.is_empty());
        assert_eq!(just_orgs.allow_orgs, vec!["kindlyhq"]);
    }
}
