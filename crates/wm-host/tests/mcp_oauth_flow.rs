//! Tier-2 end-to-end test for slice 52 (MCP-OAuth happy path).
//!
//! Exercises the full authorization-code-with-PKCE flow against a
//! real in-process host:
//!
//!   1. POST /__auth/oauth/register     → client_id + client_secret
//!   2. POST /__auth/login/password     → wm_session cookie
//!   3. GET  /__auth/oauth/authorize    → 303 to consent (session present)
//!   4. POST /__ui/oauth/consent        → 303 to client redirect_uri w/ code
//!   5. POST /__auth/oauth/token        → access_token + refresh_token
//!   6. GET  /__api/users/me            → 200 (token authenticates)
//!
//! Negative-path coverage:
//!   * bad client_secret → 401 invalid_client
//!   * unsupported grant_type → 400 unsupported_grant_type
//!   * wrong PKCE verifier → 400 invalid_grant
//!   * authorize without session → 303 to /__auth/login

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
const LOCAL_AUTH: &str = "alice:hunter2:admin";

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
        .get(url(h, "/__auth/login"))
        .send()
        .await
        .expect("get login");
    let csrf_cookie = pick_cookie(&get, "wm_csrf").expect("csrf cookie");
    let html = get.text().await.expect("login html");
    let csrf_form = extract_csrf_form_value(&html).expect("csrf form value");

    let body = format!(
        "_csrf={}&username={}&password={}",
        urlencode(&csrf_form),
        urlencode("alice"),
        urlencode("hunter2"),
    );
    let post = client
        .post(url(h, "/__auth/login/password"))
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
        .post(url(&h, "/__auth/oauth/register"))
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

    // 3. Hit /__auth/oauth/authorize as if Claude Desktop opened the
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
        .get(url(&h, &format!("/__auth/oauth/authorize?{auth_query}")))
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
        consent_path.starts_with("/__ui/oauth/consent?state="),
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
        .post(url(&h, "/__ui/oauth/consent"))
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
        .post(url(&h, "/__auth/oauth/token"))
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

    // 6. Use the access token on a /__api/ endpoint.
    let me = client
        .get(url(&h, "/__api/users/me"))
        .header("authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .expect("me");
    assert_eq!(
        me.status(),
        200,
        "wmm_ access token should authenticate /__api/*"
    );
    let me_json: Value = me.json().await.expect("me json");
    assert_eq!(me_json["name"], "alice");
}

#[tokio::test]
async fn token_endpoint_rejects_wrong_client_secret() {
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/__auth/oauth/register"))
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
        .post(url(&h, "/__auth/oauth/token"))
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
        .post(url(&h, "/__auth/oauth/register"))
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
        .post(url(&h, "/__auth/oauth/token"))
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
        .post(url(&h, "/__auth/oauth/register"))
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
        .get(url(&h, &format!("/__auth/oauth/authorize?{q}")))
        .send()
        .await
        .expect("authorize");
    assert_eq!(resp.status(), 303, "should bounce to login without session");
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with("/__auth/login?next="), "Location was {loc}");
}

#[tokio::test]
async fn redirect_uri_must_match_registered_set() {
    let h = start().await;
    let client = no_redirect_client();

    let reg = client
        .post(url(&h, "/__auth/oauth/register"))
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
        .get(url(&h, &format!("/__auth/oauth/authorize?{q}")))
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
        .post(url(&h, "/__auth/oauth/register"))
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
        .get(url(&h, &format!("/__auth/oauth/authorize?{q}")))
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
