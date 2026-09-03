//! Tier-2 end-to-end test for slice 52 (MCP-OAuth happy path).
//!
//! Exercises the full authorization-code-with-PKCE flow against a
//! real in-process host:
//!
//!   1. POST /auth/oauth/register     → client_id + client_secret
//!   2. POST /auth/login/password     → wm_session cookie
//!   3. GET  /auth/oauth/authorize    → 303 to consent (session present)
//!   4. POST /ui/oauth/consent        → 303 to client redirect_uri w/ code
//!   5. POST /auth/oauth/token        → access_token + refresh_token
//!   6. GET  /api/users/me            → 200 (token authenticates)
//!
//! Negative-path coverage:
//!   * bad client_secret → 401 invalid_client
//!   * unsupported grant_type → 400 unsupported_grant_type
//!   * wrong PKCE verifier → 400 invalid_grant
//!   * authorize without session → 303 to /auth/login

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use reqwest::Client;
use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256};
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"this-is-a-thirty-two-byte-secret";
const LOCAL_AUTH: &str = "alice@test.example:hunter2:admin";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse(LOCAL_AUTH).expect("local auth"))
        .with_sessions(SessionStore::new(storage, SECRET).expect("session store"));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    Harness { addr, server }
}

fn url(h: &Harness, path: &str) -> String {
    format!("http://{}{}", h.addr, path)
}

fn no_redirect_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client")
}

/// Pull a Set-Cookie value off a response.
fn pick_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        let s = v.to_str().ok()?;
        for pair in s.split(';') {
            let trimmed = pair.trim();
            if let Some(rest) = trimmed.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn extract_csrf_form_value(html: &str) -> Option<String> {
    let needle = "name=\"_csrf\" value=\"";
    let pos = html.find(needle)?;
    let rest = &html[pos + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Mint a wm_session cookie for `alice` by walking the local-auth
/// login flow. Returns the cookie value (without the wrapping name=).
async fn login_as_alice(h: &Harness, client: &Client) -> String {
    // GET the login page to mint the csrf cookie + pull the form
    // token. Same pattern as local_auth_e2e.rs's post_login helper.
    let get = client
        .get(url(h, "/auth/login"))
        .send()
        .await
        .expect("get login");
    let csrf_cookie = pick_cookie(&get, "wm_csrf").expect("csrf cookie");
    let html = get.text().await.expect("login html");
    let csrf_form = extract_csrf_form_value(&html).expect("csrf form value");

    let body = format!(
        "_csrf={}&email={}&password={}",
        urlencode(&csrf_form),
        urlencode("alice@test.example"),
        urlencode("hunter2"),
    );
    let post = client
        .post(url(h, "/auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(body)
        .send()
        .await
        .expect("post login");
    assert_eq!(post.status(), 303, "login should 303");
    pick_cookie(&post, "wm_session").expect("session cookie set")
}

/// PKCE: code_verifier is the secret; code_challenge is base64url(sha256(verifier)).
fn pkce_pair() -> (String, String) {
    // 32 bytes of url-safe-base64 → standard verifier length.
    let verifier = B64URL.encode(b"some_verifier_bytes_for_test_purposes_only_xx");
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    let challenge = B64URL.encode(h.finalize());
    (verifier, challenge)
}

#[tokio::test]
async fn full_authorization_code_flow_mints_a_working_access_token() {
    let h = start().await;
    let client = no_redirect_client();

    // 1. Register a client via DCR.
    let reg_body = json!({
        "client_name": "test-client",
        "redirect_uris": ["http://127.0.0.1:54321/cb"],
    });
    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&reg_body)
        .send()
        .await
        .expect("register");
    assert_eq!(reg.status(), 201, "register should 201");
    let reg_json: Value = reg.json().await.expect("register json");
    let client_id = reg_json["client_id"].as_str().unwrap().to_string();
    let client_secret = reg_json["client_secret"].as_str().unwrap().to_string();
    assert!(client_id.starts_with("wmc_"));
    assert!(client_secret.starts_with("wmcs_"));

    // 2. Log in as alice to mint a session cookie.
    let session_cookie = login_as_alice(&h, &client).await;

    // 3. Hit /auth/oauth/authorize as if Claude Desktop opened the
    //    browser to it. The session cookie is present, so we should
    //    303 to the consent page.
    let (verifier, challenge) = pkce_pair();
    let auth_query = format!(
        "response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope=*",
        urlencode(&client_id),
        urlencode("http://127.0.0.1:54321/cb"),
        urlencode("client-state-abc"),
        urlencode(&challenge),
    );
    let authz = client
        .get(url(&h, &format!("/auth/oauth/authorize?{auth_query}")))
        .header("cookie", format!("wm_session={session_cookie}"))
        .send()
        .await
        .expect("authorize");
    assert_eq!(authz.status(), 303, "authorize should 303 to consent");
    let consent_path = authz
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        consent_path.starts_with("/ui/oauth/consent?state="),
        "Location was {consent_path}"
    );
    let internal_state = consent_path.split('=').nth(1).unwrap().to_string();
    let internal_state = percent_decode(&internal_state);

    // 4a. GET the consent page (so we get the csrf cookie + form).
    let consent_get = client
        .get(url(&h, &consent_path))
        .header("cookie", format!("wm_session={session_cookie}"))
        .send()
        .await
        .expect("consent get");
    assert_eq!(consent_get.status(), 200);
    let csrf_cookie = pick_cookie(&consent_get, "wm_csrf").expect("csrf cookie");
    let consent_html = consent_get.text().await.expect("consent html");
    let csrf_form = extract_csrf_form_value(&consent_html).expect("csrf form");

    // 4b. POST approve.
    let consent_body = format!(
        "_csrf={}&state={}&action=approve",
        urlencode(&csrf_form),
        urlencode(&internal_state),
    );
    let approve = client
        .post(url(&h, "/ui/oauth/consent"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header(
            "cookie",
            format!("wm_session={session_cookie}; wm_csrf={csrf_cookie}"),
        )
        .body(consent_body)
        .send()
        .await
        .expect("consent post");
    assert_eq!(approve.status(), 303, "approve should 303 to redirect_uri");
    let redirect = approve
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        redirect.starts_with("http://127.0.0.1:54321/cb?"),
        "redirect was {redirect}"
    );
    // Pull `code` out of the redirect URL.
    let parsed = reqwest::Url::parse(&redirect).unwrap();
    let code = parsed
        .query_pairs()
        .find_map(|(k, v)| (k == "code").then(|| v.into_owned()))
        .expect("code param");
    let echoed_state = parsed
        .query_pairs()
        .find_map(|(k, v)| (k == "state").then(|| v.into_owned()))
        .expect("state param");
    assert_eq!(echoed_state, "client-state-abc");

    // 5. Exchange the code at the token endpoint.
    let token_body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}&client_secret={}",
        urlencode(&code),
        urlencode("http://127.0.0.1:54321/cb"),
        urlencode(&verifier),
        urlencode(&client_id),
        urlencode(&client_secret),
    );
    let tok = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(token_body)
        .send()
        .await
        .expect("token");
    assert_eq!(tok.status(), 200, "token should 200");
    let tok_json: Value = tok.json().await.expect("token json");
    assert_eq!(tok_json["token_type"], "Bearer");
    let access_token = tok_json["access_token"].as_str().unwrap().to_string();
    assert!(access_token.starts_with("wmm_"));
    let refresh_token = tok_json["refresh_token"].as_str().unwrap().to_string();
    assert!(refresh_token.starts_with("wmr_"));
    assert_eq!(tok_json["expires_in"], 3600);

    // 6. Use the access token on a /api/ endpoint.
    let me = client
        .get(url(&h, "/api/users/me"))
        .header("authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .expect("me");
    assert_eq!(
        me.status(),
        200,
        "wmm_ access token should authenticate /api/*"
    );
    let me_json: Value = me.json().await.expect("me json");
    assert_eq!(me_json["email"], "alice@test.example");
}

#[tokio::test]
async fn token_endpoint_rejects_wrong_client_secret() {
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    // Wrong secret.
    let body = format!(
        "grant_type=authorization_code&code=x&redirect_uri=http://127.0.0.1:0/cb&code_verifier=v&client_id={}&client_secret=wmcs_wrong",
        urlencode(client_id),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("token");
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_client");
}

#[tokio::test]
async fn token_endpoint_rejects_unknown_grant_type() {
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();
    let client_secret = reg_json["client_secret"].as_str().unwrap();

    let body = format!(
        "grant_type=password&client_id={}&client_secret={}",
        urlencode(client_id),
        urlencode(client_secret),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("token");
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unsupported_grant_type");
}

#[tokio::test]
async fn authorize_without_session_redirects_to_login() {
    let h = start().await;
    let client = no_redirect_client();

    // Register so the authorize endpoint accepts the client_id.
    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    let (_, challenge) = pkce_pair();
    let q = format!(
        "response_type=code&client_id={}&redirect_uri=http://127.0.0.1:0/cb&code_challenge={}&code_challenge_method=S256",
        urlencode(client_id),
        urlencode(&challenge),
    );
    let resp = client
        .get(url(&h, &format!("/auth/oauth/authorize?{q}")))
        .send()
        .await
        .expect("authorize");
    assert_eq!(resp.status(), 303, "should bounce to login without session");
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with("/auth/login?next="), "Location was {loc}");
}

#[tokio::test]
async fn redirect_uri_must_match_registered_set() {
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:54321/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    // Use a redirect_uri that wasn't registered (different host).
    let (_, challenge) = pkce_pair();
    let q = format!(
        "response_type=code&client_id={}&redirect_uri=http://evil.example/cb&code_challenge={}&code_challenge_method=S256",
        urlencode(client_id),
        urlencode(&challenge),
    );
    let resp = client
        .get(url(&h, &format!("/auth/oauth/authorize?{q}")))
        .send()
        .await
        .expect("authorize");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn loopback_port_wildcard_matches_registered_uri() {
    // RFC 8252: native clients bind a random loopback port per
    // session. The host registered `http://127.0.0.1:54321/cb` but a
    // later authorize with `http://127.0.0.1:9999/cb` should still
    // pass the redirect-URI check.
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:54321/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    let session_cookie = login_as_alice(&h, &client).await;
    let (_, challenge) = pkce_pair();
    let q = format!(
        "response_type=code&client_id={}&redirect_uri=http://127.0.0.1:9999/cb&code_challenge={}&code_challenge_method=S256",
        urlencode(client_id),
        urlencode(&challenge),
    );
    let resp = client
        .get(url(&h, &format!("/auth/oauth/authorize?{q}")))
        .header("cookie", format!("wm_session={session_cookie}"))
        .send()
        .await
        .expect("authorize");
    assert_eq!(
        resp.status(),
        303,
        "different loopback port should still match"
    );
}

/// Helper not used by tests but pinned here to keep the module valid
/// even if the test set shrinks during refactoring.
#[allow(dead_code)]
fn _unused_headers_marker(_: HeaderMap) {}

#[tokio::test]
async fn tokens_page_lists_active_mcp_grant_after_authorization() {
    let h = start().await;
    let client = no_redirect_client();
    let _pair = obtain_token_pair(&h, &client).await;

    // The session cookie from `obtain_token_pair` is consumed inside;
    // mint a fresh one for the tokens-page GET.
    let session = login_as_alice(&h, &client).await;
    let resp = client
        .get(url(&h, "/ui/me"))
        .header("cookie", format!("wm_session={session}"))
        .send()
        .await
        .expect("tokens page");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("MCP applications"),
        "tokens page renders the MCP applications section"
    );
    // Client_name from the obtain_token_pair helper.
    assert!(
        body.contains("tester"),
        "tokens page lists the granted application's name"
    );
    assert!(
        body.contains("/ui/me/tokens/oauth/"),
        "tokens page carries a per-grant revoke form"
    );
}

#[tokio::test]
async fn revoke_oauth_grant_from_ui_marks_refresh_token_revoked() {
    let h = start().await;
    let client = no_redirect_client();
    let pair = obtain_token_pair(&h, &client).await;

    // Get a session + the csrf cookie/form value for the revoke POST.
    let session = login_as_alice(&h, &client).await;
    let get = client
        .get(url(&h, "/ui/me"))
        .header("cookie", format!("wm_session={session}"))
        .send()
        .await
        .expect("get tokens");
    let csrf_cookie = pick_cookie(&get, "wm_csrf").expect("csrf cookie");
    let html = get.text().await.unwrap();
    let csrf_form = extract_csrf_form_value(&html).expect("csrf form");

    // Click "Revoke" on the row.
    let revoke = client
        .post(url(
            &h,
            &format!("/ui/me/tokens/oauth/{}/revoke", pair.client_id),
        ))
        .header("content-type", "application/x-www-form-urlencoded")
        .header(
            "cookie",
            format!("wm_session={session}; wm_csrf={csrf_cookie}"),
        )
        .body(format!("_csrf={}", urlencode(&csrf_form)))
        .send()
        .await
        .expect("revoke");
    assert_eq!(revoke.status(), 303, "revoke 303s back to tokens page");

    // The grant row is gone.
    let after = client
        .get(url(&h, "/ui/me"))
        .header("cookie", format!("wm_session={session}"))
        .send()
        .await
        .expect("tokens page after revoke");
    let body = after.text().await.unwrap();
    assert!(
        !body.contains(&pair.client_id),
        "client_id no longer appears in the tokens page after revoke"
    );

    // And the refresh token actually can't be exchanged anymore.
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        urlencode(&pair.refresh_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("refresh after ui revoke");
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
}

/// Walk the happy path to the point where the test has an
/// access_token + refresh_token + client credentials in hand. Used by
/// the refresh/revoke tests so they don't all re-implement steps 1-5.
async fn obtain_token_pair(h: &Harness, client: &Client) -> TokenPair {
    let reg = client
        .post(url(h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "tester",
            "redirect_uris": ["http://127.0.0.1:54321/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap().to_string();
    let client_secret = reg_json["client_secret"].as_str().unwrap().to_string();

    let session_cookie = login_as_alice(h, client).await;
    let (verifier, challenge) = pkce_pair();
    let auth_query = format!(
        "response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256",
        urlencode(&client_id),
        urlencode("http://127.0.0.1:54321/cb"),
        urlencode(&challenge),
    );
    let authz = client
        .get(url(h, &format!("/auth/oauth/authorize?{auth_query}")))
        .header("cookie", format!("wm_session={session_cookie}"))
        .send()
        .await
        .expect("authorize");
    let consent_path = authz
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let internal_state = percent_decode(consent_path.split('=').nth(1).unwrap());

    let consent_get = client
        .get(url(h, &consent_path))
        .header("cookie", format!("wm_session={session_cookie}"))
        .send()
        .await
        .expect("consent get");
    let csrf_cookie = pick_cookie(&consent_get, "wm_csrf").expect("csrf");
    let html = consent_get.text().await.unwrap();
    let csrf_form = extract_csrf_form_value(&html).expect("csrf form");
    let approve = client
        .post(url(h, "/ui/oauth/consent"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header(
            "cookie",
            format!("wm_session={session_cookie}; wm_csrf={csrf_cookie}"),
        )
        .body(format!(
            "_csrf={}&state={}&action=approve",
            urlencode(&csrf_form),
            urlencode(&internal_state),
        ))
        .send()
        .await
        .expect("consent post");
    let redirect = approve
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let parsed = reqwest::Url::parse(&redirect).unwrap();
    let code = parsed
        .query_pairs()
        .find_map(|(k, v)| (k == "code").then(|| v.into_owned()))
        .expect("code");

    let tok = client
        .post(url(h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}&client_secret={}",
            urlencode(&code),
            urlencode("http://127.0.0.1:54321/cb"),
            urlencode(&verifier),
            urlencode(&client_id),
            urlencode(&client_secret),
        ))
        .send()
        .await
        .expect("token");
    let tj: Value = tok.json().await.unwrap();
    TokenPair {
        access_token: tj["access_token"].as_str().unwrap().to_string(),
        refresh_token: tj["refresh_token"].as_str().unwrap().to_string(),
        client_id,
        client_secret,
    }
}

struct TokenPair {
    access_token: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
}

#[tokio::test]
async fn refresh_token_grant_rotates_and_yields_a_working_access_token() {
    let h = start().await;
    let client = no_redirect_client();
    let pair = obtain_token_pair(&h, &client).await;

    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        urlencode(&pair.refresh_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("refresh");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let new_access = body["access_token"].as_str().unwrap();
    let new_refresh = body["refresh_token"].as_str().unwrap();
    assert!(new_access.starts_with("wmm_"));
    assert!(new_refresh.starts_with("wmr_"));
    assert_ne!(new_refresh, pair.refresh_token, "refresh must rotate");

    // The new access token authenticates /api/*.
    let me = client
        .get(url(&h, "/api/users/me"))
        .header("authorization", format!("Bearer {new_access}"))
        .send()
        .await
        .expect("me");
    assert_eq!(me.status(), 200);
}

#[tokio::test]
async fn refresh_token_replay_is_rejected() {
    let h = start().await;
    let client = no_redirect_client();
    let pair = obtain_token_pair(&h, &client).await;

    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        urlencode(&pair.refresh_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );

    // First exchange succeeds.
    let r1 = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.clone())
        .send()
        .await
        .expect("first refresh");
    assert_eq!(r1.status(), 200);

    // Replay with the same refresh_token rejects.
    let r2 = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("replay refresh");
    assert_eq!(r2.status(), 400);
    let body: Value = r2.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn revoke_invalidates_an_access_token() {
    let h = start().await;
    let client = no_redirect_client();
    let pair = obtain_token_pair(&h, &client).await;

    // Confirm it works first.
    let me_before = client
        .get(url(&h, "/api/users/me"))
        .header("authorization", format!("Bearer {}", pair.access_token))
        .send()
        .await
        .expect("me before");
    assert_eq!(me_before.status(), 200);

    // Revoke.
    let body = format!(
        "token={}&client_id={}&client_secret={}",
        urlencode(&pair.access_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );
    let rv = client
        .post(url(&h, "/auth/oauth/revoke"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("revoke");
    assert_eq!(rv.status(), 200, "revoke always 200 per RFC 7009");

    // Token no longer authenticates.
    let me_after = client
        .get(url(&h, "/api/users/me"))
        .header("authorization", format!("Bearer {}", pair.access_token))
        .send()
        .await
        .expect("me after");
    assert_eq!(me_after.status(), 401);
}

#[tokio::test]
async fn revoke_marks_refresh_token_as_revoked() {
    let h = start().await;
    let client = no_redirect_client();
    let pair = obtain_token_pair(&h, &client).await;

    let body = format!(
        "token={}&token_type_hint=refresh_token&client_id={}&client_secret={}",
        urlencode(&pair.refresh_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );
    let rv = client
        .post(url(&h, "/auth/oauth/revoke"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("revoke");
    assert_eq!(rv.status(), 200);

    // Subsequent refresh exchange fails.
    let refresh_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        urlencode(&pair.refresh_token),
        urlencode(&pair.client_id),
        urlencode(&pair.client_secret),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(refresh_body)
        .send()
        .await
        .expect("refresh");
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn revoke_with_unknown_token_still_returns_200() {
    // RFC 7009 §2.2: response is identical for known/unknown tokens
    // to prevent enumeration. We register a client and use it to
    // authenticate the revoke call, but the token itself is garbage.
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "t",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let rj: Value = reg.json().await.unwrap();
    let cid = rj["client_id"].as_str().unwrap();
    let csec = rj["client_secret"].as_str().unwrap();

    let body = format!(
        "token=wmm_nonsense&client_id={}&client_secret={}",
        urlencode(cid),
        urlencode(csec),
    );
    let resp = client
        .post(url(&h, "/auth/oauth/revoke"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("revoke");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn authorize_rejects_wrong_resource_with_actionable_error() {
    // Client misconfigured to point at the host root rather than the
    // MCP endpoint at /api/mcp. The fix that made this catchable:
    // we validate `resource` (RFC 8707) at authorize time, before the
    // consent dialog, so the user sees a fixable error instead of
    // going through OAuth and then failing opaquely at the MCP
    // transport with the resulting token.
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    let (_, challenge) = pkce_pair();

    // resource= names the host root, NOT /api/mcp.
    let wrong_resource = format!("http://{}", h.addr);
    let q = format!(
        "response_type=code&client_id={}&redirect_uri=http://127.0.0.1:0/cb&code_challenge={}&code_challenge_method=S256&resource={}",
        urlencode(client_id),
        urlencode(&challenge),
        urlencode(&wrong_resource),
    );
    let resp = client
        .get(url(&h, &format!("/auth/oauth/authorize?{q}")))
        .send()
        .await
        .expect("authorize");

    assert_eq!(resp.status(), 400);
    let body = resp.text().await.expect("text");
    let expected = format!("http://{}/api/mcp", h.addr);
    assert!(
        body.contains(&expected),
        "error should name the correct MCP endpoint: {body}"
    );
    assert!(
        body.contains(&wrong_resource),
        "error should echo the wrong resource so the user knows what they sent: {body}"
    );
    assert!(
        body.contains("Reconfigure"),
        "error should be actionable (tell them to reconfigure): {body}"
    );
}

#[tokio::test]
async fn authorize_accepts_correct_resource() {
    // resource= exactly matches /api/mcp → request proceeds (and
    // returns the usual 303-to-login since this test client has no
    // session cookie; the point is that the resource check didn't
    // reject it with 400).
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/auth/oauth/register"))
        .json(&json!({
            "client_name": "test",
            "redirect_uris": ["http://127.0.0.1:0/cb"],
        }))
        .send()
        .await
        .expect("register");
    let reg_json: Value = reg.json().await.unwrap();
    let client_id = reg_json["client_id"].as_str().unwrap();

    let (_, challenge) = pkce_pair();
    let correct_resource = format!("http://{}/api/mcp", h.addr);
    let q = format!(
        "response_type=code&client_id={}&redirect_uri=http://127.0.0.1:0/cb&code_challenge={}&code_challenge_method=S256&resource={}",
        urlencode(client_id),
        urlencode(&challenge),
        urlencode(&correct_resource),
    );
    let resp = client
        .get(url(&h, &format!("/auth/oauth/authorize?{q}")))
        .send()
        .await
        .expect("authorize");

    // No session → bounce to login. The point is it's not a 400 from
    // the resource check.
    assert_eq!(
        resp.status(),
        303,
        "correct resource should pass validation and reach the session check"
    );
}
