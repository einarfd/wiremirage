//! Generic OIDC user-login flow (ADR-0035).
//!
//! One relying-party implementation covers every OIDC-compliant IdP
//! (Pocket ID, Keycloak, Authentik, Zitadel, Dex, Okta, Google, ...):
//! the operator configures an issuer URL and client credentials, and
//! everything else comes from the issuer's discovery document at
//! `{issuer}/.well-known/openid-configuration`.
//!
//! Identity comes from the **userinfo endpoint**, not from validating
//! the ID-token JWT. For a confidential client doing the code flow
//! over TLS directly to the issuer, the userinfo response carries the
//! same trust as the token-endpoint response itself, and skipping JWT
//! validation keeps this module on reqwest + serde with no JWKS
//! machinery — see ADR-0035's alternatives for the full argument.
//!
//! Configuration is env-only, fail-fast on partial config (mirrors
//! [`crate::github_oauth`]): `WM_OIDC_ISSUER` + `WM_OIDC_CLIENT_ID` +
//! `WM_OIDC_CLIENT_SECRET`, an allow posture — `WM_OIDC_ALLOW_ALL=true`
//! (private IdP: the issuer's user base is the allow-list) or at least
//! one per-identity rule (`WM_OIDC_ALLOW_EMAILS` /
//! `WM_OIDC_ALLOW_DOMAINS` / `WM_OIDC_ALLOW_GROUPS`), never both —
//! optional admin promotion (`WM_OIDC_ADMIN_EMAILS` /
//! `WM_OIDC_ADMIN_GROUPS`), plus `WM_OIDC_DISPLAY_NAME`,
//! `WM_OIDC_GROUPS_CLAIM`, and `WM_OIDC_EXTRA_SCOPES` knobs.

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// Operator-supplied configuration, parsed from env before the
/// discovery fetch. [`OidcConfig::discover`] turns it into a live
/// [`OidcProvider`].
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Issuer URL, e.g. `https://id.example.com`. The discovery
    /// document is fetched from
    /// `{issuer}/.well-known/openid-configuration` and its `issuer`
    /// field must match (modulo a trailing slash).
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Login-button label, e.g. "Pocket ID". Defaults to the issuer's
    /// host so an unconfigured label is still meaningful.
    pub display_name: String,
    /// Every user the issuer authenticates is allowed — the right
    /// posture for a *private* IdP (Pocket ID, closed-registration
    /// Keycloak, corporate Okta) where account existence already IS
    /// the authorization decision, and per-app restrictions live in
    /// the IdP. Explicit opt-in; mutually exclusive with the
    /// per-identity rules below so the config states one intent.
    /// Never set this against a public issuer (Google, ...) — there
    /// "authenticated" means anyone on the internet.
    pub allow_all: bool,
    /// Exact emails allowed to log in. OR'd with the other allow rules.
    pub allow_emails: Vec<String>,
    /// Email domains allowed to log in (matched against the part
    /// after `@`).
    pub allow_domains: Vec<String>,
    /// Values of the groups claim allowed to log in.
    pub allow_groups: Vec<String>,
    /// Emails promoted to `is_admin = true` on login.
    pub admin_emails: Vec<String>,
    /// Groups promoted to `is_admin = true` on login.
    pub admin_groups: Vec<String>,
    /// Name of the userinfo claim carrying group membership. The one
    /// genuinely non-standard corner of OIDC — most self-hosted IdPs
    /// call it `groups` (our default) but the name is IdP-configured.
    pub groups_claim: String,
    /// Scopes appended to the base `openid profile email` (some IdPs
    /// gate the groups claim behind an extra scope).
    pub extra_scopes: Vec<String>,
}

/// Endpoints resolved from the issuer's discovery document.
/// Constructed by [`OidcConfig::discover`] in production; tests build
/// one directly to point at an in-process mock issuer.
#[derive(Debug, Clone)]
pub struct OidcEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: String,
    /// Whether the token endpoint advertises `client_secret_basic`.
    /// When false we fall back to `client_secret_post` (credentials
    /// in the form body), the other of the two REQUIRED-to-pick
    /// methods in the spec.
    pub token_auth_basic: bool,
}

/// A configured provider with resolved endpoints — what the auth
/// handlers actually use.
#[derive(Debug, Clone)]
pub struct OidcProvider {
    pub config: OidcConfig,
    pub endpoints: OidcEndpoints,
}

impl OidcConfig {
    /// Parse `WM_OIDC_*` from env. Returns `Ok(None)` when
    /// `WM_OIDC_ISSUER` is unset (operator hasn't opted in); `Err` on
    /// partial or contradictory config so a typo surfaces at startup.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_env_values(EnvValues {
            issuer: env::var("WM_OIDC_ISSUER").ok(),
            client_id: env::var("WM_OIDC_CLIENT_ID").ok(),
            client_secret: env::var("WM_OIDC_CLIENT_SECRET").ok(),
            display_name: env::var("WM_OIDC_DISPLAY_NAME").ok(),
            allow_all: env::var("WM_OIDC_ALLOW_ALL").ok(),
            allow_emails: env::var("WM_OIDC_ALLOW_EMAILS").unwrap_or_default(),
            allow_domains: env::var("WM_OIDC_ALLOW_DOMAINS").unwrap_or_default(),
            allow_groups: env::var("WM_OIDC_ALLOW_GROUPS").unwrap_or_default(),
            admin_emails: env::var("WM_OIDC_ADMIN_EMAILS").unwrap_or_default(),
            admin_groups: env::var("WM_OIDC_ADMIN_GROUPS").unwrap_or_default(),
            groups_claim: env::var("WM_OIDC_GROUPS_CLAIM").ok(),
            extra_scopes: env::var("WM_OIDC_EXTRA_SCOPES").unwrap_or_default(),
        })
    }

    /// Pure-function variant of `from_env` — same rationale as the
    /// GitHub module: unit tests cover the parsing rules without
    /// racing other tests on the process environment.
    fn from_env_values(values: EnvValues) -> Result<Option<Self>> {
        let Some(issuer) = values.issuer.filter(|s| !s.is_empty()) else {
            // Issuer unset → OIDC login not requested. Credentials
            // without an issuer are a misconfiguration worth naming.
            if values.client_id.as_deref().is_some_and(|s| !s.is_empty())
                || values
                    .client_secret
                    .as_deref()
                    .is_some_and(|s| !s.is_empty())
            {
                return Err(anyhow!(
                    "WM_OIDC_CLIENT_ID/WM_OIDC_CLIENT_SECRET are set but WM_OIDC_ISSUER is not; \
                     set the issuer URL or unset the credentials"
                ));
            }
            return Ok(None);
        };
        if !issuer.starts_with("https://") && !issuer.starts_with("http://") {
            return Err(anyhow!(
                "WM_OIDC_ISSUER must be an http(s) URL, got {issuer:?}"
            ));
        }
        // Normalize: discovery paths and issuer-match compares both
        // want the no-trailing-slash form.
        let issuer = issuer.trim_end_matches('/').to_string();

        let client_id = values.client_id.filter(|s| !s.is_empty());
        let client_secret = values.client_secret.filter(|s| !s.is_empty());
        let (client_id, client_secret) = match (client_id, client_secret) {
            (Some(id), Some(secret)) => (id, secret),
            _ => {
                return Err(anyhow!(
                    "WM_OIDC_ISSUER is set but WM_OIDC_CLIENT_ID and WM_OIDC_CLIENT_SECRET \
                     must both be set too"
                ));
            }
        };

        let allow_all = match values.allow_all.as_deref() {
            None | Some("") => false,
            Some(v)
                if v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("on") =>
            {
                true
            }
            Some(other) => {
                return Err(anyhow!(
                    "WM_OIDC_ALLOW_ALL must be true/1/on (or unset), got {other:?}"
                ));
            }
        };
        let allow_emails = parse_csv(&values.allow_emails);
        let allow_domains = parse_csv(&values.allow_domains);
        let allow_groups = parse_csv(&values.allow_groups);
        let has_identity_rules =
            !allow_emails.is_empty() || !allow_domains.is_empty() || !allow_groups.is_empty();
        if allow_all && has_identity_rules {
            // Redundant config usually means the operator thinks the
            // per-identity rules still restrict something. State one
            // intent or the other.
            return Err(anyhow!(
                "WM_OIDC_ALLOW_ALL=true admits every user the issuer authenticates, \
                 which makes WM_OIDC_ALLOW_EMAILS/_DOMAINS/_GROUPS meaningless. \
                 Set either WM_OIDC_ALLOW_ALL or the per-identity rules, not both."
            ));
        }
        if !allow_all && !has_identity_rules {
            return Err(anyhow!(
                "OIDC login is configured but no allow rules are set. \
                 Set WM_OIDC_ALLOW_EMAILS=a@x.com,b@x.com and/or \
                 WM_OIDC_ALLOW_DOMAINS=x.com and/or WM_OIDC_ALLOW_GROUPS=team — \
                 or WM_OIDC_ALLOW_ALL=true if this is a private IdP whose whole \
                 user base should be allowed (account existence as the \
                 authorization decision). Without one, no account can log in \
                 (refusing to start so this misconfiguration surfaces fast \
                 rather than silently denying every login)."
            ));
        }

        let display_name = values
            .display_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| issuer_host(&issuer));

        Ok(Some(Self {
            issuer,
            client_id,
            client_secret,
            display_name,
            allow_all,
            allow_emails,
            allow_domains,
            allow_groups,
            admin_emails: parse_csv(&values.admin_emails),
            admin_groups: parse_csv(&values.admin_groups),
            groups_claim: values
                .groups_claim
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "groups".to_string()),
            extra_scopes: parse_csv(&values.extra_scopes),
        }))
    }

    /// Fetch `{issuer}/.well-known/openid-configuration` and resolve
    /// the endpoints. Called once at startup; failure refuses startup
    /// (an unreachable IdP would 503 every login anyway, and surfacing
    /// it here beats surfacing it to the first user who clicks the
    /// button).
    pub async fn discover(self) -> Result<OidcProvider> {
        let url = format!("{}/.well-known/openid-configuration", self.issuer);
        let client = reqwest_client()?;
        let resp = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("fetch OIDC discovery document: {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("OIDC discovery endpoint {url} returned {status}"));
        }
        let doc: DiscoveryDoc = resp
            .json()
            .await
            .with_context(|| format!("parse OIDC discovery document: {url}"))?;

        // The spec requires the advertised issuer to match the one the
        // document was fetched from. A mismatch means the operator
        // pointed us at the wrong URL (or a proxy is rewriting) — the
        // classic trailing-slash / wrong-tenant failure, caught here
        // instead of as a mystery login loop.
        if doc.issuer.trim_end_matches('/') != self.issuer {
            return Err(anyhow!(
                "OIDC discovery issuer mismatch: WM_OIDC_ISSUER is {:?} but the \
                 discovery document says {:?}. Set WM_OIDC_ISSUER to exactly the \
                 issuer the IdP advertises.",
                self.issuer,
                doc.issuer
            ));
        }
        let userinfo_endpoint = doc.userinfo_endpoint.ok_or_else(|| {
            anyhow!(
                "OIDC discovery document from {url} has no userinfo_endpoint; \
                 WireMirage's OIDC login reads identity from userinfo (ADR-0035) \
                 and can't work against this IdP"
            )
        })?;
        // Absent per spec means client_secret_basic.
        let token_auth_basic = doc
            .token_endpoint_auth_methods_supported
            .map(|methods| methods.iter().any(|m| m == "client_secret_basic"))
            .unwrap_or(true);

        Ok(OidcProvider {
            config: self,
            endpoints: OidcEndpoints {
                authorization_endpoint: doc.authorization_endpoint,
                token_endpoint: doc.token_endpoint,
                userinfo_endpoint,
                token_auth_basic,
            },
        })
    }
}

impl OidcProvider {
    /// Build the URL we redirect the user's browser to. `state` is the
    /// CSRF nonce and `pkce_challenge` the S256 challenge — the caller
    /// persists the nonce + verifier server-side and validates the
    /// round-trip on callback.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str, pkce_challenge: &str) -> String {
        let mut scope = "openid profile email".to_string();
        for extra in &self.config.extra_scopes {
            scope.push(' ');
            scope.push_str(extra);
        }
        let mut url = reqwest::Url::parse(&self.endpoints.authorization_endpoint)
            .expect("authorization_endpoint is a valid URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("scope", &scope)
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256");
        url.into()
    }

    /// Exchange the authorization code for an access token. Client
    /// authentication follows what discovery advertised —
    /// `client_secret_basic` (HTTP Basic) when supported, else
    /// `client_secret_post` (credentials in the form body).
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        pkce_verifier: &str,
    ) -> Result<String> {
        let client = reqwest_client()?;
        let mut fake = reqwest::Url::parse("https://placeholder/").expect("placeholder");
        {
            let mut pairs = fake.query_pairs_mut();
            pairs
                .append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("code_verifier", pkce_verifier);
            if !self.endpoints.token_auth_basic {
                pairs
                    .append_pair("client_id", &self.config.client_id)
                    .append_pair("client_secret", &self.config.client_secret);
            }
        }
        let body = fake.query().unwrap_or("").to_string();

        let mut req = client
            .post(&self.endpoints.token_endpoint)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body);
        if self.endpoints.token_auth_basic {
            req = req.basic_auth(&self.config.client_id, Some(&self.config.client_secret));
        }
        let resp = req.send().await.context("call OIDC token endpoint")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parse token response JSON")?;
        if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
            let desc = body
                .get("error_description")
                .and_then(|v| v.as_str())
                .unwrap_or("(no description)");
            return Err(anyhow!("OIDC token endpoint: {err} ({desc})"));
        }
        if !status.is_success() {
            return Err(anyhow!("OIDC token endpoint returned {status}: {body}"));
        }
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("OIDC token response missing access_token: {body}"))?;
        Ok(token.to_string())
    }

    /// Fetch the user's identity from the userinfo endpoint.
    pub async fn fetch_identity(&self, access_token: &str) -> Result<OidcIdentity> {
        let client = reqwest_client()?;
        let resp = client
            .get(&self.endpoints.userinfo_endpoint)
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .context("call OIDC userinfo endpoint")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OIDC userinfo endpoint returned {status}: {body}"));
        }
        let claims: serde_json::Value =
            resp.json().await.context("parse userinfo response JSON")?;
        OidcIdentity::from_claims(&claims, &self.config.groups_claim)
    }

    /// Apply the allow rules. `Ok(())` when the identity matches at
    /// least one rule; `Err` with a reason that names what the user
    /// *does* have, so an operator reading logs can fix the rule.
    pub fn check_allow(&self, identity: &OidcIdentity) -> Result<(), AllowFailure> {
        if self.config.allow_all {
            // Private-IdP posture: the issuer's user base is the
            // allow-list; per-app restrictions live in the IdP.
            return Ok(());
        }
        if let Some(email) = &identity.email {
            let email_lc = email.to_ascii_lowercase();
            if self
                .config
                .allow_emails
                .iter()
                .any(|e| e.to_ascii_lowercase() == email_lc)
            {
                return Ok(());
            }
            if let Some(domain) = email_lc.rsplit_once('@').map(|(_, d)| d)
                && self
                    .config
                    .allow_domains
                    .iter()
                    .any(|d| d.to_ascii_lowercase() == domain)
            {
                return Ok(());
            }
        }
        for group in &identity.groups {
            let group_lc = group.to_ascii_lowercase();
            if self
                .config
                .allow_groups
                .iter()
                .any(|g| g.to_ascii_lowercase() == group_lc)
            {
                return Ok(());
            }
        }
        Err(AllowFailure {
            subject: identity.subject.clone(),
            email: identity.email.clone(),
            groups: identity.groups.clone(),
        })
    }

    /// Whether this identity should be promoted to admin on login.
    /// Operator opts in via `WM_OIDC_ADMIN_EMAILS` / `_ADMIN_GROUPS`;
    /// unset means no OIDC user is admin.
    pub fn is_admin(&self, identity: &OidcIdentity) -> bool {
        if let Some(email) = &identity.email {
            let email_lc = email.to_ascii_lowercase();
            if self
                .config
                .admin_emails
                .iter()
                .any(|e| e.to_ascii_lowercase() == email_lc)
            {
                return true;
            }
        }
        identity.groups.iter().any(|group| {
            let group_lc = group.to_ascii_lowercase();
            self.config
                .admin_groups
                .iter()
                .any(|g| g.to_ascii_lowercase() == group_lc)
        })
    }
}

/// Pre-resolved env values fed into the pure-function
/// `from_env_values`.
#[derive(Default)]
struct EnvValues {
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    display_name: Option<String>,
    allow_all: Option<String>,
    allow_emails: String,
    allow_domains: String,
    allow_groups: String,
    admin_emails: String,
    admin_groups: String,
    groups_claim: Option<String>,
    extra_scopes: String,
}

/// Identity assembled from userinfo claims.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    /// The `sub` claim — the issuer-stable subject we persist as the
    /// `(provider, subject)` tuple.
    pub subject: String,
    /// The `email` claim, dropped when the IdP explicitly marked it
    /// `email_verified: false` — an unverified email must not satisfy
    /// email/domain allow rules. Absent `email_verified` is treated
    /// as verified (several IdPs omit the claim entirely).
    pub email: Option<String>,
    /// Values of the configured groups claim; empty when the claim is
    /// absent. Accepts both an array of strings and a single string
    /// (a real cross-IdP variance).
    pub groups: Vec<String>,
}

impl OidcIdentity {
    fn from_claims(claims: &serde_json::Value, groups_claim: &str) -> Result<Self> {
        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("userinfo response has no sub claim: {claims}"))?
            .to_string();
        let email_verified = claims
            .get("email_verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .filter(|_| email_verified)
            .map(str::to_string);
        let groups = match claims.get(groups_claim) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect(),
            Some(serde_json::Value::String(one)) => vec![one.clone()],
            _ => vec![],
        };
        Ok(Self {
            subject,
            email,
            groups,
        })
    }
}

/// A user authenticated at the IdP but matched no allow rule.
#[derive(Debug, Clone)]
pub struct AllowFailure {
    pub subject: String,
    /// The verified email we saw, if any — so the operator can tell
    /// "wrong domain" from "no verified email".
    pub email: Option<String>,
    /// The groups the user IS in, so the operator can see whether
    /// they meant to allow one of them.
    pub groups: Vec<String>,
}

impl std::fmt::Display for AllowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OIDC subject {subject:?} is not on this host's allow-list",
            subject = self.subject
        )?;
        match &self.email {
            Some(email) => write!(f, " (email: {email}")?,
            None => write!(f, " (no verified email")?,
        }
        if self.groups.is_empty() {
            write!(f, ", no groups)")
        } else {
            write!(f, ", groups: {})", self.groups.join(", "))
        }
    }
}

/// Generate a PKCE verifier + S256 challenge pair (RFC 7636). The
/// verifier is 32 random bytes urlsafe-base64'd (43 chars, inside the
/// spec's 43–128 window); the challenge is the urlsafe-base64 of its
/// SHA-256.
pub fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use rand::RngCore as _;
    use sha2::Digest as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// Discovery-document fields we consume. The document has dozens more;
/// serde ignores them.
#[derive(Debug, Deserialize)]
struct DiscoveryDoc {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

fn issuer_host(issuer: &str) -> String {
    reqwest::Url::parse(issuer)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| issuer.to_string())
}

/// Same short-timeout client shape as the GitHub module — these are
/// user-blocking requests and a wedged IdP shouldn't pin a worker.
fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("wm-host/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")
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
    use serde_json::json;

    fn cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "cid".into(),
            client_secret: "csec".into(),
            display_name: "Example ID".into(),
            allow_all: false,
            allow_emails: vec!["alice@corp.example".into()],
            allow_domains: vec!["acme.example".into()],
            allow_groups: vec!["mockers".into()],
            admin_emails: vec!["alice@corp.example".into()],
            admin_groups: vec!["wm-admins".into()],
            groups_claim: "groups".into(),
            extra_scopes: vec![],
        }
    }

    fn provider(config: OidcConfig) -> OidcProvider {
        OidcProvider {
            config,
            endpoints: OidcEndpoints {
                authorization_endpoint: "https://id.example.com/authorize".into(),
                token_endpoint: "https://id.example.com/token".into(),
                userinfo_endpoint: "https://id.example.com/userinfo".into(),
                token_auth_basic: true,
            },
        }
    }

    fn identity(email: Option<&str>, groups: Vec<&str>) -> OidcIdentity {
        OidcIdentity {
            subject: "sub-1".into(),
            email: email.map(str::to_string),
            groups: groups.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn authorize_url_carries_pkce_and_scopes() {
        let p = provider(cfg());
        let url = p.authorize_url("http://localhost:8080/auth/callback/oidc", "st", "chal");
        assert!(url.starts_with("https://id.example.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=st"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=openid+profile+email"));
    }

    #[test]
    fn authorize_url_appends_extra_scopes() {
        let mut c = cfg();
        c.extra_scopes = vec!["groups".into()];
        let url = provider(c).authorize_url("http://x/cb", "st", "ch");
        assert!(url.contains("scope=openid+profile+email+groups"), "{url}");
    }

    #[test]
    fn allow_passes_on_email_domain_or_group() {
        let p = provider(cfg());
        assert!(
            p.check_allow(&identity(Some("alice@corp.example"), vec![]))
                .is_ok()
        );
        assert!(
            p.check_allow(&identity(Some("bob@acme.example"), vec![]))
                .is_ok()
        );
        assert!(p.check_allow(&identity(None, vec!["mockers"])).is_ok());
    }

    #[test]
    fn allow_is_case_insensitive() {
        let p = provider(cfg());
        assert!(
            p.check_allow(&identity(Some("ALICE@CORP.EXAMPLE"), vec![]))
                .is_ok()
        );
        assert!(
            p.check_allow(&identity(Some("bob@ACME.example"), vec![]))
                .is_ok()
        );
        assert!(p.check_allow(&identity(None, vec!["MOCKERS"])).is_ok());
    }

    #[test]
    fn allow_denies_and_names_what_the_user_has() {
        let p = provider(cfg());
        let err = p
            .check_allow(&identity(Some("eve@other.example"), vec!["randos"]))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("eve@other.example"), "{msg}");
        assert!(msg.contains("randos"), "{msg}");
    }

    #[test]
    fn allow_all_admits_any_identity() {
        let mut c = cfg();
        c.allow_all = true;
        c.allow_emails.clear();
        c.allow_domains.clear();
        c.allow_groups.clear();
        let p = provider(c);
        assert!(p.check_allow(&identity(None, vec![])).is_ok());
        assert!(
            p.check_allow(&identity(Some("anyone@anywhere.example"), vec![]))
                .is_ok()
        );
    }

    #[test]
    fn allow_denies_without_email_or_groups() {
        let p = provider(cfg());
        let err = p.check_allow(&identity(None, vec![])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no verified email"), "{msg}");
        assert!(msg.contains("no groups"), "{msg}");
    }

    #[test]
    fn admin_by_email_or_group() {
        let p = provider(cfg());
        assert!(p.is_admin(&identity(Some("alice@corp.example"), vec![])));
        assert!(p.is_admin(&identity(None, vec!["wm-admins"])));
        assert!(!p.is_admin(&identity(Some("bob@acme.example"), vec!["mockers"])));
    }

    #[test]
    fn identity_drops_explicitly_unverified_email() {
        let claims = json!({
            "sub": "s1",
            "email": "eve@acme.example",
            "email_verified": false
        });
        let id = OidcIdentity::from_claims(&claims, "groups").unwrap();
        assert_eq!(id.email, None);
    }

    #[test]
    fn identity_keeps_email_when_verified_flag_absent() {
        let claims = json!({ "sub": "s1", "email": "a@b.c" });
        let id = OidcIdentity::from_claims(&claims, "groups").unwrap();
        assert_eq!(id.email.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn identity_reads_groups_array_and_single_string() {
        let arr = json!({ "sub": "s1", "groups": ["a", "b"] });
        let id = OidcIdentity::from_claims(&arr, "groups").unwrap();
        assert_eq!(id.groups, vec!["a", "b"]);

        let single = json!({ "sub": "s1", "roles": "admin" });
        let id = OidcIdentity::from_claims(&single, "roles").unwrap();
        assert_eq!(id.groups, vec!["admin"]);
    }

    #[test]
    fn identity_requires_sub() {
        let claims = json!({ "email": "a@b.c" });
        assert!(OidcIdentity::from_claims(&claims, "groups").is_err());
    }

    #[test]
    fn pkce_pair_is_s256_of_verifier() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 43);
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expect);
    }

    // Pure-function tests on `from_env_values`.

    #[test]
    fn from_env_returns_none_when_unconfigured() {
        assert!(
            OidcConfig::from_env_values(EnvValues::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn from_env_rejects_credentials_without_issuer() {
        let err = OidcConfig::from_env_values(EnvValues {
            client_id: Some("x".into()),
            ..EnvValues::default()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("WM_OIDC_ISSUER"));
    }

    #[test]
    fn from_env_rejects_partial_credentials() {
        let err = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: None,
            allow_domains: "acme.example".into(),
            ..EnvValues::default()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("WM_OIDC_CLIENT_SECRET"));
    }

    #[test]
    fn from_env_rejects_when_no_allow_rules() {
        let err = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            ..EnvValues::default()
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("WM_OIDC_ALLOW_"), "{msg}");
    }

    #[test]
    fn from_env_accepts_allow_all_alone() {
        let cfg = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_all: Some("true".into()),
            ..EnvValues::default()
        })
        .unwrap()
        .unwrap();
        assert!(cfg.allow_all);
    }

    #[test]
    fn from_env_rejects_allow_all_combined_with_identity_rules() {
        let err = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_all: Some("true".into()),
            allow_domains: "acme.example".into(),
            ..EnvValues::default()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("not both"));
    }

    #[test]
    fn from_env_rejects_garbage_allow_all_value() {
        let err = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_all: Some("banana".into()),
            ..EnvValues::default()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("WM_OIDC_ALLOW_ALL"));
    }

    #[test]
    fn from_env_rejects_non_url_issuer() {
        let err = OidcConfig::from_env_values(EnvValues {
            issuer: Some("id.example.com".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_domains: "acme.example".into(),
            ..EnvValues::default()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("http"));
    }

    #[test]
    fn from_env_normalizes_and_defaults() {
        let cfg = OidcConfig::from_env_values(EnvValues {
            issuer: Some("https://id.example.com/".into()),
            client_id: Some("x".into()),
            client_secret: Some("y".into()),
            allow_domains: "acme.example".into(),
            ..EnvValues::default()
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            cfg.issuer, "https://id.example.com",
            "trailing slash trimmed"
        );
        assert_eq!(
            cfg.display_name, "id.example.com",
            "display name defaults to host"
        );
        assert_eq!(cfg.groups_claim, "groups");
    }
}
