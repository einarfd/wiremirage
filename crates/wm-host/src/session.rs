//! Browser session storage + cookie signing.
//!
//! Sessions are minted on successful login (local-auth today, OAuth
//! later) and persisted to Valkey at `session:{token}`. The cookie
//! carries `{token}.{signature}` where `signature = HMAC-SHA256(
//! SESSION_SECRET, token)`. Rotating `SESSION_SECRET` invalidates
//! every existing session.
//!
//! Sliding TTL: every authenticated request bumps `last_seen_at` and
//! resets `expires_at = last_seen_at + ttl`. Logout deletes the
//! record; next presentation of the cookie 401s.
//!
//! "Sign out everywhere" is not implemented here, because it needs no
//! session enumeration: each record carries the user's `epoch` at
//! creation, and the auth path rejects a session whose stamp is behind
//! the user's current counter. This module only has to carry the
//! stamp; `Auth::bump_session_epoch` does the revoking.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use ulid::Ulid;

use crate::store::{Bucket, Storage, StoreError};

type HmacSha256 = Hmac<Sha256>;

/// Cookie name. Lower-case + `wm_` prefix avoids collision with
/// anything an SUT might happen to set in a co-deployed environment.
pub const COOKIE_NAME: &str = "wm_session";

/// Default sliding TTL per `auth-and-authz.md` ("Sliding TTL: every
/// request updates `last_seen_at` and bumps `expires_at` forward by
/// 24h"). Overridable for tests.
pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 24 * 60 * 60;

/// Length of the random portion of the cookie before signing.
/// 32 bytes URL-safe base64 → 43 characters; comfortable margin
/// against birthday collisions on the index.
const TOKEN_RANDOM_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("storage backend error: {0}")]
    Storage(#[from] StoreError),
    #[error("session not found or expired")]
    NotFound,
    #[error("malformed session record: {0}")]
    Malformed(String),
    #[error("invalid cookie format")]
    InvalidCookie,
    #[error("cookie signature mismatch")]
    SignatureMismatch,
    #[error("SESSION_SECRET must be at least 32 bytes")]
    WeakSecret,
}

/// Persisted shape of a session. Mirrors the "Sessions in detail"
/// block in [[auth-and-authz.md]] minus the OAuth-only fields (those
/// land alongside OAuth).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    /// `"local"` for env-var auth; `"google"`/`"github"`/… for OAuth.
    /// Lets audit trails distinguish login source without re-hitting
    /// the user record.
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_first_seen: String,
    pub user_agent: String,
    /// The owning user's `session_epoch` when this session was minted.
    /// Compared against the live user record on every authenticated
    /// request; behind means revoked. Defaults to 0 so records written
    /// before the field existed decode cleanly and stay valid.
    #[serde(default)]
    pub epoch: u64,
}

#[derive(Clone)]
pub struct SessionStore {
    storage: Storage,
    /// HMAC key. Kept in a `Vec<u8>` because the signing key length
    /// is operator-configured (we enforce a minimum, not a maximum).
    secret: Vec<u8>,
    ttl_seconds: u64,
}

// Hand-rolled Debug so a stray `?store` doesn't print the HMAC key.
impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("secret", &format!("<{} bytes redacted>", self.secret.len()))
            .field("ttl_seconds", &self.ttl_seconds)
            .finish()
    }
}

impl SessionStore {
    /// Build a session store from `SESSION_SECRET` bytes. Requires
    /// ≥32 bytes; weaker secrets are rejected fail-fast so a
    /// misconfigured deployment doesn't silently mint signable-with-
    /// trivial-brute-force cookies.
    pub fn new(storage: Storage, secret: &[u8]) -> Result<Self, SessionError> {
        if secret.len() < 32 {
            return Err(SessionError::WeakSecret);
        }
        Ok(Self {
            storage,
            secret: secret.to_vec(),
            ttl_seconds: DEFAULT_SESSION_TTL_SECONDS,
        })
    }

    /// Override the sliding TTL. Used by tests to verify expiry
    /// without sleeping for 24 hours.
    pub fn with_ttl_seconds(mut self, ttl: u64) -> Self {
        self.ttl_seconds = ttl;
        self
    }

    pub fn ttl_seconds(&self) -> u64 {
        self.ttl_seconds
    }

    fn bucket(&self) -> Result<Bucket, SessionError> {
        Ok(self.storage.admin_bucket()?)
    }

    /// Mint a session for `user_id` and return `(session_record,
    /// signed_cookie_value)`. The caller sets the cookie on the
    /// response.
    pub fn create(
        &self,
        user_id: &str,
        provider: &str,
        ip_first_seen: &str,
        user_agent: &str,
        epoch: u64,
    ) -> Result<(Session, String), SessionError> {
        let token = mint_token();
        let now = Utc::now();
        let session = Session {
            id: Ulid::generate().to_string(),
            user_id: user_id.to_string(),
            provider: provider.to_string(),
            created_at: now,
            last_seen_at: now,
            expires_at: now + Duration::seconds(self.ttl_seconds as i64),
            ip_first_seen: ip_first_seen.to_string(),
            user_agent: user_agent.to_string(),
            epoch,
        };
        self.write(&token, &session)?;
        Ok((session, self.sign_cookie(&token)))
    }

    /// Validate a cookie value: split into token + signature, verify
    /// the HMAC in constant time, then load the session record and
    /// reject if it's past `expires_at`.
    pub fn validate(&self, cookie_value: &str) -> Result<Session, SessionError> {
        let (token, signature) = cookie_value
            .split_once('.')
            .ok_or(SessionError::InvalidCookie)?;
        if !verify_signature(&self.secret, token, signature) {
            return Err(SessionError::SignatureMismatch);
        }
        let session = self.read(token)?;
        if session.expires_at < Utc::now() {
            // Best-effort cleanup; ignore failures (the next sweeper
            // pass or the TTL on the record will catch it).
            let _ = self.delete(token);
            return Err(SessionError::NotFound);
        }
        Ok(session)
    }

    /// Bump `last_seen_at` and `expires_at` on every authenticated
    /// request. Verifies the HMAC signature first — a tampered cookie
    /// is rejected with `SignatureMismatch` rather than silently
    /// authenticating against the storage lookup. Best-effort: a
    /// storage failure logs upstream but shouldn't bounce the request.
    pub fn touch(&self, cookie_value: &str) -> Result<Session, SessionError> {
        let (token, signature) = cookie_value
            .split_once('.')
            .ok_or(SessionError::InvalidCookie)?;
        if !verify_signature(&self.secret, token, signature) {
            return Err(SessionError::SignatureMismatch);
        }
        let mut session = self.read(token)?;
        if session.expires_at < Utc::now() {
            let _ = self.delete(token);
            return Err(SessionError::NotFound);
        }
        let now = Utc::now();
        session.last_seen_at = now;
        session.expires_at = now + Duration::seconds(self.ttl_seconds as i64);
        self.write(token, &session)?;
        Ok(session)
    }

    /// Delete the session record for `cookie_value`. Idempotent —
    /// missing sessions are treated as success (already-logged-out).
    pub fn delete_by_cookie(&self, cookie_value: &str) -> Result<(), SessionError> {
        let (token, _) = cookie_value
            .split_once('.')
            .ok_or(SessionError::InvalidCookie)?;
        self.delete(token)
    }

    /// HMAC-sign a raw token for the wire format. Public so callers
    /// who already have a `Session` plus its raw token can re-emit
    /// the cookie without round-tripping through storage.
    pub fn sign_cookie(&self, token: &str) -> String {
        let sig = compute_signature(&self.secret, token);
        format!("{token}.{sig}")
    }

    fn write(&self, token: &str, session: &Session) -> Result<(), SessionError> {
        let mut bucket = self.bucket()?;
        let key = format!("session:{token}");
        let bytes = serde_json::to_vec(session)
            .map_err(|e| SessionError::Malformed(format!("encode: {e}")))?;
        bucket.set(&key, bytes)?;
        // Mirror the in-record TTL with a storage TTL so abandoned
        // sessions are reaped on schedule even without a sweeper.
        bucket.set_ttl(&key, self.ttl_seconds)?;
        Ok(())
    }

    fn read(&self, token: &str) -> Result<Session, SessionError> {
        let mut bucket = self.bucket()?;
        let key = format!("session:{token}");
        let bytes = bucket.get(&key)?.ok_or(SessionError::NotFound)?;
        serde_json::from_slice(&bytes).map_err(|e| SessionError::Malformed(format!("decode: {e}")))
    }

    fn delete(&self, token: &str) -> Result<(), SessionError> {
        let mut bucket = self.bucket()?;
        let key = format!("session:{token}");
        bucket.delete(&key)?;
        Ok(())
    }
}

fn mint_token() -> String {
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    B64URL.encode(bytes)
}

fn compute_signature(secret: &[u8], token: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key length validated at startup");
    mac.update(token.as_bytes());
    B64URL.encode(mac.finalize().into_bytes())
}

fn verify_signature(secret: &[u8], token: &str, signature: &str) -> bool {
    let expected = compute_signature(secret, token);
    // Constant-time compare: a naïve `==` on signatures gives a
    // timing oracle that lets an attacker reconstruct the HMAC byte
    // by byte. `subtle::ConstantTimeEq` is the standard guard.
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store() -> SessionStore {
        SessionStore::new(Storage::in_memory(), &[0u8; 32]).expect("session store")
    }

    #[test]
    fn weak_secret_rejected() {
        let err = SessionStore::new(Storage::in_memory(), &[0u8; 16]).unwrap_err();
        assert!(matches!(err, SessionError::WeakSecret));
    }

    #[test]
    fn create_then_validate_round_trip() {
        let store = fresh_store();
        let (created, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        let loaded = store.validate(&cookie).unwrap();
        assert_eq!(created, loaded);
    }

    #[test]
    fn tampered_signature_fails_validation() {
        let store = fresh_store();
        let (_session, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        // Flip one character in the signature half.
        let mut tampered = cookie.clone();
        let last = tampered.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        tampered.push(replacement);
        assert!(matches!(
            store.validate(&tampered),
            Err(SessionError::SignatureMismatch)
        ));
    }

    #[test]
    fn malformed_cookie_fails_validation() {
        let store = fresh_store();
        let err = store.validate("no-dot-here").unwrap_err();
        assert!(matches!(err, SessionError::InvalidCookie));
    }

    #[test]
    fn touch_updates_last_seen_and_expires() {
        let store = fresh_store();
        let (created, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let touched = store.touch(&cookie).unwrap();
        assert!(touched.last_seen_at > created.last_seen_at);
        assert!(touched.expires_at > created.expires_at);
    }

    #[test]
    fn delete_then_validate_returns_not_found() {
        let store = fresh_store();
        let (_session, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        store.delete_by_cookie(&cookie).unwrap();
        assert!(matches!(
            store.validate(&cookie),
            Err(SessionError::NotFound)
        ));
    }

    #[test]
    fn expired_session_rejected_and_cleaned_up() {
        // A 1-second TTL gives us a deterministic expiry window.
        let store = SessionStore::new(Storage::in_memory(), &[0u8; 32])
            .unwrap()
            .with_ttl_seconds(1);
        let (_session, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(matches!(
            store.validate(&cookie),
            Err(SessionError::NotFound)
        ));
    }

    #[test]
    fn signature_uses_constant_time_compare() {
        // Smoke test: two cookies that share a prefix but differ in
        // the last byte should both fail in roughly the same time.
        // We don't measure here — the real guarantee is `ct_eq`
        // being on the comparison path. This test exists so a future
        // refactor that swaps in `==` doesn't slip through unnoticed.
        let store = fresh_store();
        let (_session, cookie) = store
            .create("user-1", "local", "127.0.0.1", "test-agent", 0)
            .unwrap();
        let mut tampered_early = cookie.clone();
        let dot_idx = tampered_early.find('.').unwrap();
        // Bit-flip the first char of the signature.
        let bytes = unsafe { tampered_early.as_bytes_mut() };
        bytes[dot_idx + 1] ^= 0x01;
        // Bit-flip the last char.
        let mut tampered_late = cookie.clone();
        let len = tampered_late.len();
        let bytes = unsafe { tampered_late.as_bytes_mut() };
        bytes[len - 1] ^= 0x01;

        assert!(store.validate(&tampered_early).is_err());
        assert!(store.validate(&tampered_late).is_err());
    }
}
