//! Local user accounts via the `WM_LOCAL_AUTH` env var.
//!
//! ADR-0018 scopes the feature: a deliberately-small email/password
//! mechanism for testing and trusted-network deployments. Not for
//! public exposure — see the ADR for the threat model.
//!
//! Format: comma-separated entries, each `email:password:role`
//! where `role` is `admin` or `user` (default `user`). Accounts are
//! keyed by email (user-model.md), so the identifier must contain
//! `@` — nothing is ever *sent* to it:
//!
//! ```text
//! WM_LOCAL_AUTH=alice@corp.example:hunter2:admin,bob@corp.example:correct-horse-battery-staple
//! ```
//!
//! Plaintext lives only in the env var. The host parses + argon2-hashes
//! at startup, keeps the hashes in memory, and never persists or logs
//! them. Restart re-parses; removing an entry blocks the next login but
//! doesn't touch the existing user record.

use std::collections::HashMap;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::RngCore;
use thiserror::Error;

/// 16 bytes is the recommended argon2 salt length per OWASP. We use
/// `rand`'s `OsRng` (already a wm-host dependency) rather than the
/// rand_core re-export from argon2 — the latter requires enabling a
/// feature flag we'd otherwise have no reason to turn on.
fn fresh_salt() -> SaltString {
    // 16 bytes is the OWASP-recommended argon2 salt length. We use
    // `rand::rng()` (the same source `auth.rs` uses for token
    // generation) rather than argon2's re-exported rand_core, which
    // would require enabling a feature flag — different rand_core
    // versions live in the workspace already.
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    SaltString::encode_b64(&bytes).expect("salt encoding fits the buffer")
}

/// Per-user role declared in `WM_LOCAL_AUTH`. Mirrors the host's
/// `User.is_admin` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRole {
    Admin,
    User,
}

impl LocalRole {
    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

#[derive(Clone)]
struct Credential {
    /// PHC-encoded argon2id hash (`$argon2id$v=19$m=...$...`).
    /// Embedding the parameters in the hash lets us upgrade the
    /// argon2 cost factors later without breaking existing hashes.
    hash: String,
    role: LocalRole,
}

// Hand-rolled Debug so a stray `Debug` of `LocalAuth` (e.g. via
// `tracing::debug!(?auth)`) doesn't dump the PHC hash strings —
// they're not plaintext, but redacting them is good hygiene.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("hash", &"<redacted>")
            .field("role", &self.role)
            .finish()
    }
}

/// In-memory map of locally-configured users. Built once from the env
/// var at startup; never mutated afterwards.
#[derive(Debug, Clone, Default)]
pub struct LocalAuth {
    users: HashMap<String, Credential>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty entry in WM_LOCAL_AUTH (extra comma?)")]
    EmptyEntry,
    #[error("entry missing password: {0:?}")]
    MissingPassword(String),
    #[error(
        "user identifier {0:?} is not an email address (accounts are keyed by email; \
         nothing is ever sent to it)"
    )]
    NotAnEmail(String),
    #[error("empty password for user {0:?}")]
    EmptyPassword(String),
    #[error("invalid role for user {user:?}: {raw:?} (use `admin` or `user`)")]
    InvalidRole { user: String, raw: String },
    #[error("duplicate email in WM_LOCAL_AUTH: {0:?}")]
    DuplicateEmail(String),
}

#[derive(Debug, Error)]
pub enum VerifyError {
    /// User isn't in the env var (or hash decode failed — opaque so we
    /// don't leak which side of the lookup missed).
    #[error("invalid credentials")]
    Invalid,
    /// argon2 verify failed internally — not the user's fault. Logged
    /// at the call site; the caller still gets a generic 401.
    #[error("argon2 verification error: {0}")]
    HashEngine(String),
}

impl LocalAuth {
    /// Empty — no users configured. Login attempts always fail.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a `WM_LOCAL_AUTH` value and hash every password. Returns
    /// the populated map. Whitespace-only input parses as empty (the
    /// caller decides whether to construct `Self::empty()` or skip
    /// the call).
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let mut users = HashMap::new();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        for entry in trimmed.split(',') {
            // `entry.split(':')` — but we can't split-then-trim
            // because a password might legitimately contain spaces.
            // Trim only the entry's surrounding whitespace.
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(ParseError::EmptyEntry);
            }
            let (email, rest) = entry
                .split_once(':')
                .ok_or_else(|| ParseError::MissingPassword(entry.to_string()))?;
            // Normalized the same way auth.rs normalizes emails, so the
            // login lookup and the user record agree.
            let email = email.trim().to_ascii_lowercase();
            if email.is_empty() || !email.contains('@') {
                return Err(ParseError::NotAnEmail(email));
            }
            // `rest` is `password` or `password:role`. We split on the
            // *last* `:` so a password containing `:` works as long as
            // the role suffix is present (or absent entirely). When
            // `rest.contains(':')`, we treat the suffix as the role
            // candidate; if it's `admin`/`user`, that's the role and
            // everything before is the password. If the suffix isn't
            // a valid role, the whole `rest` is the password and the
            // role defaults to user.
            let (password, role) = match rest.rsplit_once(':') {
                Some((pw, role_candidate)) => match parse_role(role_candidate) {
                    Some(role) => (pw, role),
                    // Trailing colons that aren't `:admin` or `:user`
                    // are surfaced as an error. Silently dropping
                    // them would mask config bugs ("I typed `:adimn`").
                    None => {
                        return Err(ParseError::InvalidRole {
                            user: email.clone(),
                            raw: role_candidate.to_string(),
                        });
                    }
                },
                None => (rest, LocalRole::User),
            };
            if password.is_empty() {
                return Err(ParseError::EmptyPassword(email));
            }
            if users.contains_key(&email) {
                return Err(ParseError::DuplicateEmail(email));
            }
            let hash = hash_password(password).map_err(|_| {
                // argon2 hashing failure on startup is a host bug, not
                // a config bug. Surface it as InvalidRole-ish so the
                // operator sees *something* — but in practice this
                // arm is unreachable.
                ParseError::InvalidRole {
                    user: email.clone(),
                    raw: "<hash-failure>".to_string(),
                }
            })?;
            users.insert(email, Credential { hash, role });
        }
        Ok(Self { users })
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    pub fn has_user(&self, email: &str) -> bool {
        self.users.contains_key(&email.trim().to_ascii_lowercase())
    }

    /// Look up the role for `email`. Used after a successful
    /// verify so the caller knows what `is_admin` to write on the
    /// user record. Returns `None` for unknown users.
    pub fn role(&self, email: &str) -> Option<LocalRole> {
        self.users
            .get(&email.trim().to_ascii_lowercase())
            .map(|c| c.role)
    }

    /// Verify a plaintext password against the stored hash for
    /// `email`. Returns `Ok(())` on match, `Err(VerifyError::Invalid)`
    /// for unknown user OR wrong password (we don't distinguish on
    /// purpose — leaking "user exists" doesn't help anyone in this
    /// threat model).
    pub fn verify(&self, email: &str, password: &str) -> Result<LocalRole, VerifyError> {
        let email = email.trim().to_ascii_lowercase();
        let credential = self.users.get(&email).ok_or(VerifyError::Invalid)?;
        let parsed = PasswordHash::new(&credential.hash)
            .map_err(|e| VerifyError::HashEngine(e.to_string()))?;
        match Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => Ok(credential.role),
            Err(argon2::password_hash::Error::Password) => Err(VerifyError::Invalid),
            Err(e) => Err(VerifyError::HashEngine(e.to_string())),
        }
    }
}

fn parse_role(raw: &str) -> Option<LocalRole> {
    match raw.trim() {
        "admin" => Some(LocalRole::Admin),
        "user" => Some(LocalRole::User),
        _ => None,
    }
}

/// argon2id hash of `password` with library defaults. PHC-encoded so
/// the verifier can pull the parameters out at check time.
///
/// Pub so the OAuth dynamic-client-registration flow can hash the
/// client_secret using the same parameters as user passwords.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = fresh_salt();
    let hash = Argon2::default().hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

/// Verify a plaintext value against a PHC-encoded argon2 hash.
/// Returns `true` on match, `false` on either mismatch or malformed
/// hash — callers that need to distinguish should parse the hash
/// themselves before calling. Used by the OAuth token endpoint to
/// authenticate registered clients (same shape as `LocalAuth::verify`,
/// minus the username lookup).
pub fn verify_password(stored_hash: &str, plaintext: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_env_var_parses_to_empty_map() {
        let auth = LocalAuth::parse("").unwrap();
        assert!(auth.is_empty());
        let auth = LocalAuth::parse("   ").unwrap();
        assert!(auth.is_empty());
    }

    #[test]
    fn single_user_no_role_defaults_to_user() {
        let auth = LocalAuth::parse("alice@corp.example:hunter2").unwrap();
        assert!(auth.has_user("alice@corp.example"));
        assert_eq!(auth.role("alice@corp.example"), Some(LocalRole::User));
        assert!(matches!(
            auth.verify("alice@corp.example", "hunter2"),
            Ok(LocalRole::User)
        ));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let auth = LocalAuth::parse("Alice@Corp.Example:hunter2").unwrap();
        assert!(auth.has_user("alice@corp.example"));
        assert!(matches!(
            auth.verify("ALICE@corp.example", "hunter2"),
            Ok(LocalRole::User)
        ));
    }

    #[test]
    fn non_email_identifier_rejected() {
        let err = LocalAuth::parse("alice:hunter2").unwrap_err();
        assert!(matches!(err, ParseError::NotAnEmail(_)));
    }

    #[test]
    fn admin_role_recognized() {
        let auth = LocalAuth::parse("alice@corp.example:hunter2:admin").unwrap();
        assert!(matches!(
            auth.verify("alice@corp.example", "hunter2"),
            Ok(LocalRole::Admin)
        ));
    }

    #[test]
    fn explicit_user_role_same_as_default() {
        let auth = LocalAuth::parse("alice@corp.example:hunter2:user").unwrap();
        assert_eq!(auth.role("alice@corp.example"), Some(LocalRole::User));
    }

    #[test]
    fn multiple_users_parsed() {
        let auth =
            LocalAuth::parse("alice@corp.example:hunter2:admin,bob@corp.example:correct-horse")
                .unwrap();
        assert_eq!(auth.role("alice@corp.example"), Some(LocalRole::Admin));
        assert_eq!(auth.role("bob@corp.example"), Some(LocalRole::User));
    }

    #[test]
    fn password_with_colons_works_when_role_suffix_present() {
        // `secret:contains:colons:admin` → role=admin, password="secret:contains:colons"
        let auth = LocalAuth::parse("alice@corp.example:secret:contains:colons:admin").unwrap();
        assert!(matches!(
            auth.verify("alice@corp.example", "secret:contains:colons"),
            Ok(LocalRole::Admin)
        ));
    }

    #[test]
    fn wrong_password_returns_invalid() {
        let auth = LocalAuth::parse("alice@corp.example:hunter2").unwrap();
        assert!(matches!(
            auth.verify("alice@corp.example", "wrong"),
            Err(VerifyError::Invalid)
        ));
    }

    #[test]
    fn unknown_user_returns_invalid() {
        let auth = LocalAuth::parse("alice@corp.example:hunter2").unwrap();
        assert!(matches!(
            auth.verify("eve@corp.example", "hunter2"),
            Err(VerifyError::Invalid)
        ));
    }

    #[test]
    fn empty_identifier_rejected() {
        let err = LocalAuth::parse(":hunter2").unwrap_err();
        assert!(matches!(err, ParseError::NotAnEmail(_)));
    }

    #[test]
    fn missing_password_rejected() {
        let err = LocalAuth::parse("alice@corp.example").unwrap_err();
        assert!(matches!(err, ParseError::MissingPassword(_)));
    }

    #[test]
    fn empty_password_rejected() {
        let err = LocalAuth::parse("alice@corp.example:").unwrap_err();
        assert!(matches!(err, ParseError::EmptyPassword(_)));
    }

    #[test]
    fn invalid_role_rejected() {
        let err = LocalAuth::parse("alice@corp.example:hunter2:wizard").unwrap_err();
        assert!(matches!(err, ParseError::InvalidRole { .. }));
    }

    #[test]
    fn duplicate_email_rejected() {
        let err = LocalAuth::parse("alice@corp.example:a,Alice@corp.example:b").unwrap_err();
        assert!(matches!(err, ParseError::DuplicateEmail(_)));
    }

    #[test]
    fn empty_entry_from_extra_comma_rejected() {
        let err = LocalAuth::parse("alice@corp.example:a,,bob@corp.example:b").unwrap_err();
        assert!(matches!(err, ParseError::EmptyEntry));
    }
}
