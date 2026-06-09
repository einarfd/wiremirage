//! Authentication via bearer tokens.
//!
//! Storage layout per `storage-model.md`:
//!
//!   user:{ulid}                          hash with user fields
//!   user:by-name:{name}                  string -> user ulid
//!   user:all                             set of all user ulids
//!
//!   token:{ulid}                         hash with token fields (incl. hash; never plaintext)
//!   token:by-hash:{sha256-hex}           string -> token ulid (fast auth lookup)
//!   token:by-name:{owner_ulid}:{name}    string -> token ulid (revoke by name)
//!   token:by-owner:{owner_ulid}          set of token ulids
//!
//! Token plaintext format `wmt_<base64-no-padding-of-32-random-bytes>` per
//! ADR-0012. We never persist plaintext; only its SHA-256 (hex). Token
//! authenticate compares hashes.
//!
//! Slice 5 scope: bearer-token auth + user CRUD on top. OAuth login
//! flow, sessions, and identity linking land in a follow-up slice.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use thiserror::Error;
use ulid::Ulid;

use crate::store::{Bucket, Storage, StoreError};

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("storage backend error: {0}")]
    Storage(#[from] StoreError),
    #[error("user or token not found")]
    NotFound,
    #[error("name already in use: {0}")]
    NameTaken(String),
    #[error("malformed record in storage: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    /// SHA-256 of the plaintext, hex-encoded. Plaintext is never stored.
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Scopes granted to this token. v1 always issues `["*"]` (full
    /// access of the owner); the field is reserved per ADR-0012 so
    /// v0.2 can wire up enforcement without a data-shape change.
    pub scopes: Vec<String>,
}

/// The scope value granted to every v1 token. Once scope enforcement
/// lands (v0.2), token creation will accept a narrower set; until then
/// `["*"]` means "every permission the owner has."
pub const FULL_ACCESS_SCOPES: &[&str] = &["*"];

fn default_scopes() -> Vec<String> {
    FULL_ACCESS_SCOPES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Which credential satisfied the auth check. Lets downstream
/// handlers branch on token-vs-session if needed (e.g. forbid
/// session-only callers from a programmatic-only endpoint). Today
/// all `/api/*` handlers treat both the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Token,
    Session,
}

/// Caller identity resolved from a successful credential check.
/// Carries the credential's id (token or session) opaquely so audit
/// logs can correlate without exposing plaintext.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: String,
    pub user_name: String,
    pub is_admin: bool,
    pub credential_kind: CredentialKind,
    pub credential_id: String,
}

#[derive(Clone)]
pub struct Auth {
    storage: Storage,
}

impl Auth {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Borrow the underlying storage. Callers that need to write
    /// auxiliary auth-scoped state (OAuth flow nonces, etc.) use
    /// this to reach `admin_bucket()` without going through every
    /// `Auth::method` shape.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    fn bucket(&self) -> Result<Bucket, AuthError> {
        Ok(self.storage.admin_bucket()?)
    }

    // -- Bootstrap --------------------------------------------------------

    /// Idempotently ensure an admin user exists with `name`, owning a
    /// token derived from the supplied plaintext. Returns `true` if a
    /// new user was created, `false` if the user already existed.
    pub fn bootstrap_admin(&self, name: &str, plaintext: &str) -> Result<bool, AuthError> {
        let mut bucket = self.bucket()?;
        if let Some(_existing) = self.read_user_by_name(&mut bucket, name)? {
            return Ok(false);
        }
        let user = self.write_new_user(&mut bucket, name, true)?;
        self.write_new_token(&mut bucket, &user.id, "bootstrap", plaintext, None)?;
        Ok(true)
    }

    // -- User CRUD --------------------------------------------------------

    /// Returns `true` if at least one user exists. Used by the host's
    /// startup gate: a fresh deployment with no bootstrap token has no
    /// way to authenticate, so we refuse to start in that case.
    pub fn any_user_exists(&self) -> Result<bool, AuthError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members("user:all")?;
        Ok(!ids.is_empty())
    }

    pub fn create_user(&self, name: &str, is_admin: bool) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        if self.read_user_by_name(&mut bucket, name)?.is_some() {
            return Err(AuthError::NameTaken(name.to_string()));
        }
        self.write_new_user(&mut bucket, name, is_admin)
    }

    pub fn get_user_by_id(&self, id: &str) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        self.read_user_by_id(&mut bucket, id)?
            .ok_or(AuthError::NotFound)
    }

    pub fn get_user_by_name(&self, name: &str) -> Result<Option<User>, AuthError> {
        let mut bucket = self.bucket()?;
        self.read_user_by_name(&mut bucket, name)
    }

    pub fn list_users(&self) -> Result<Vec<User>, AuthError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members("user:all")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(user) = self.read_user_by_id(&mut bucket, &id)? {
                out.push(user);
            }
        }
        Ok(out)
    }

    /// Count of users with `is_admin == true`. Used by the API to refuse
    /// operations that would leave the system with zero admins.
    pub fn count_admins(&self) -> Result<usize, AuthError> {
        Ok(self.list_users()?.iter().filter(|u| u.is_admin).count())
    }

    /// Upsert a user backed by a local-auth (env-var) identity.
    /// First call creates the user; subsequent calls sync `is_admin`
    /// from the env-var role (per ADR-0018: "Admin role lives in the
    /// env var"). The username is the user record's `name` and also
    /// the `subject` on the implicit `local:` identity index.
    pub fn upsert_local_user(&self, username: &str, is_admin: bool) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        if let Some(mut user) = self.read_user_by_name(&mut bucket, username)? {
            if user.is_admin != is_admin {
                bucket.hash_set(
                    &format!("user:{}", user.id),
                    "is_admin",
                    if is_admin { b"1" } else { b"0" }.to_vec(),
                )?;
                user.is_admin = is_admin;
            }
            // Refresh the identity index even if the record exists —
            // protects against the (unusual) case where the entry was
            // wiped manually but the user record survived.
            bucket.set(
                &format!("user:by-identity:local:{username}"),
                user.id.as_bytes().to_vec(),
            )?;
            return Ok(user);
        }
        let user = self.write_new_user(&mut bucket, username, is_admin)?;
        bucket.set(
            &format!("user:by-identity:local:{username}"),
            user.id.as_bytes().to_vec(),
        )?;
        Ok(user)
    }

    /// Upsert a user keyed on an external identity (e.g. a GitHub
    /// numeric ID). Returning users are looked up via
    /// `user:by-identity:{provider}:{subject}` so the user's display
    /// name and admin flag survive renames on the provider side.
    ///
    /// `name_hint` is the GitHub login (or equivalent). It's used as
    /// the user record's `name` on first sight. If a user with that
    /// name already exists and isn't linked to this `(provider,
    /// subject)`, returns `NameTaken` — operator resolves manually
    /// (rename the colliding user, or delete it). In single-operator
    /// deployments this is rare; the bootstrap user is "bootstrap"
    /// and most GitHub logins don't collide.
    pub fn upsert_oauth_user(
        &self,
        provider: &str,
        subject: &str,
        name_hint: &str,
        is_admin: bool,
    ) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        let identity_key = format!("user:by-identity:{provider}:{subject}");

        // Already-linked path: returning user. Sync `is_admin` from
        // the latest env-var config so removing someone from the
        // admin list at boot-time strips their admin role on next
        // login, matching the local-auth contract.
        if let Some(bytes) = bucket.get(&identity_key)? {
            let id = String::from_utf8(bytes)
                .map_err(|_| AuthError::Malformed("identity index value".into()))?;
            if let Some(mut user) = self.read_user_by_id(&mut bucket, &id)? {
                if user.is_admin != is_admin {
                    bucket.hash_set(
                        &format!("user:{}", user.id),
                        "is_admin",
                        if is_admin { b"1" } else { b"0" }.to_vec(),
                    )?;
                    user.is_admin = is_admin;
                }
                return Ok(user);
            }
            // Stale identity index pointing at a deleted user record.
            // Drop the stale entry and fall through to create.
            bucket.delete(&identity_key)?;
        }

        // First-login path: must not collide with an existing user
        // record (which would be e.g. the bootstrap user or a
        // local-auth user). Surface as NameTaken — the operator can
        // delete or rename to resolve.
        if let Some(_existing) = self.read_user_by_name(&mut bucket, name_hint)? {
            return Err(AuthError::NameTaken(name_hint.to_string()));
        }

        let user = self.write_new_user(&mut bucket, name_hint, is_admin)?;
        bucket.set(&identity_key, user.id.as_bytes().to_vec())?;
        Ok(user)
    }

    /// Toggle a user's admin flag. Idempotent — setting to the current
    /// value is a no-op.
    pub fn set_user_admin(&self, id: &str, is_admin: bool) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        let mut user = self
            .read_user_by_id(&mut bucket, id)?
            .ok_or(AuthError::NotFound)?;
        user.is_admin = is_admin;
        bucket.hash_set(
            &format!("user:{}", user.id),
            "is_admin",
            if is_admin { b"1" } else { b"0" }.to_vec(),
        )?;
        Ok(user)
    }

    /// Delete a user and cascade-delete every token they own. Routes
    /// they own are left untouched — the API layer is expected to refuse
    /// the delete upstream when `list_routes_by_owner` is non-empty.
    pub fn delete_user(&self, id: &str) -> Result<(), AuthError> {
        let mut bucket = self.bucket()?;
        let user = self
            .read_user_by_id(&mut bucket, id)?
            .ok_or(AuthError::NotFound)?;

        // Cascade tokens. `revoke_token_by_name` rebuilds the bucket
        // each call (cheap on the in-memory backend, one connection
        // per call on Valkey); for the per-user counts we expect this
        // is fine. If user-delete becomes hot, push the loop into a
        // single bucket borrow.
        let token_ids = bucket.set_members(&format!("token:by-owner:{}", user.id))?;
        for token_id in token_ids {
            if let Some(token) = self.read_token_by_id(&mut bucket, &token_id)? {
                self.delete_token(&mut bucket, &token)?;
            }
        }

        bucket.delete(&format!("user:by-name:{}", user.name))?;
        bucket.set_remove("user:all", &user.id)?;
        for field in ["id", "name", "is_admin", "created_at"] {
            bucket.hash_delete(&format!("user:{}", user.id), field)?;
        }
        Ok(())
    }

    fn read_user_by_id(&self, bucket: &mut Bucket, id: &str) -> Result<Option<User>, AuthError> {
        let fields = bucket.hash_get_all(&format!("user:{id}"))?;
        if fields.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_user(&fields)?))
    }

    fn read_user_by_name(
        &self,
        bucket: &mut Bucket,
        name: &str,
    ) -> Result<Option<User>, AuthError> {
        let Some(bytes) = bucket.get(&format!("user:by-name:{name}"))? else {
            return Ok(None);
        };
        let id = String::from_utf8(bytes)
            .map_err(|_| AuthError::Malformed("user:by-name value".into()))?;
        self.read_user_by_id(bucket, &id)
    }

    fn write_new_user(
        &self,
        bucket: &mut Bucket,
        name: &str,
        is_admin: bool,
    ) -> Result<User, AuthError> {
        let user = User {
            id: Ulid::new().to_string(),
            name: name.to_string(),
            is_admin,
            created_at: Utc::now(),
        };
        let key = format!("user:{}", user.id);
        bucket.hash_set(&key, "id", user.id.as_bytes().to_vec())?;
        bucket.hash_set(&key, "name", user.name.as_bytes().to_vec())?;
        bucket.hash_set(
            &key,
            "is_admin",
            if user.is_admin { b"1" } else { b"0" }.to_vec(),
        )?;
        bucket.hash_set(
            &key,
            "created_at",
            user.created_at.to_rfc3339().into_bytes(),
        )?;
        bucket.set(
            &format!("user:by-name:{}", user.name),
            user.id.as_bytes().to_vec(),
        )?;
        bucket.set_add("user:all", &user.id)?;
        Ok(user)
    }

    // -- Token CRUD --------------------------------------------------------

    /// Create a new token for `owner_id`. Returns the persisted record
    /// and the plaintext token (only available here — never persisted).
    pub fn create_token(
        &self,
        owner_id: &str,
        name: &str,
        ttl_seconds: Option<u64>,
    ) -> Result<(Token, String), AuthError> {
        let mut bucket = self.bucket()?;
        // Owner must exist.
        if self.read_user_by_id(&mut bucket, owner_id)?.is_none() {
            return Err(AuthError::NotFound);
        }
        // Names are unique per owner.
        if bucket
            .get(&format!("token:by-name:{owner_id}:{name}"))?
            .is_some()
        {
            return Err(AuthError::NameTaken(name.to_string()));
        }
        let plaintext = generate_plaintext_token();
        let token = self.write_new_token(&mut bucket, owner_id, name, &plaintext, ttl_seconds)?;
        Ok((token, plaintext))
    }

    fn write_new_token(
        &self,
        bucket: &mut Bucket,
        owner_id: &str,
        name: &str,
        plaintext: &str,
        ttl_seconds: Option<u64>,
    ) -> Result<Token, AuthError> {
        let hash = sha256_hex(plaintext);
        let now = Utc::now();
        let token = Token {
            id: Ulid::new().to_string(),
            name: name.to_string(),
            owner_id: owner_id.to_string(),
            hash: hash.clone(),
            created_at: now,
            last_used_at: None,
            expires_at: ttl_seconds.map(|s| now + Duration::seconds(s as i64)),
            scopes: default_scopes(),
        };
        let key = format!("token:{}", token.id);
        bucket.hash_set(&key, "id", token.id.as_bytes().to_vec())?;
        bucket.hash_set(&key, "name", token.name.as_bytes().to_vec())?;
        bucket.hash_set(&key, "owner_id", token.owner_id.as_bytes().to_vec())?;
        bucket.hash_set(&key, "hash", token.hash.as_bytes().to_vec())?;
        bucket.hash_set(
            &key,
            "created_at",
            token.created_at.to_rfc3339().into_bytes(),
        )?;
        if let Some(ts) = token.expires_at {
            bucket.hash_set(&key, "expires_at", ts.to_rfc3339().into_bytes())?;
        }
        bucket.hash_set(&key, "scopes", token.scopes.join(" ").into_bytes())?;

        bucket.set(
            &format!("token:by-hash:{hash}"),
            token.id.as_bytes().to_vec(),
        )?;
        bucket.set(
            &format!("token:by-name:{owner_id}:{name}"),
            token.id.as_bytes().to_vec(),
        )?;
        bucket.set_add(&format!("token:by-owner:{owner_id}"), &token.id)?;
        Ok(token)
    }

    /// Look up one of `owner_id`'s tokens by name. Returns `Ok(None)` if
    /// the owner has no token with that name.
    pub fn get_token_by_name(
        &self,
        owner_id: &str,
        name: &str,
    ) -> Result<Option<Token>, AuthError> {
        let mut bucket = self.bucket()?;
        let Some(bytes) = bucket.get(&format!("token:by-name:{owner_id}:{name}"))? else {
            return Ok(None);
        };
        let token_id = String::from_utf8(bytes)
            .map_err(|_| AuthError::Malformed("token:by-name value".into()))?;
        self.read_token_by_id(&mut bucket, &token_id)
    }

    pub fn list_tokens_for(&self, owner_id: &str) -> Result<Vec<Token>, AuthError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members(&format!("token:by-owner:{owner_id}"))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = self.read_token_by_id(&mut bucket, &id)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Rename one of `owner_id`'s tokens. The hashed secret and metadata
    /// (created_at, expires_at, last_used_at) are preserved; only the
    /// human-readable name changes. The plaintext token continues to
    /// authenticate.
    ///
    /// Errors:
    ///   * `NotFound` — no token with `old_name`
    ///   * `NameTaken(new_name)` — owner already has a token with that name
    ///   * `Malformed` — bad storage shape (corruption)
    pub fn rename_token(
        &self,
        owner_id: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<Token, AuthError> {
        let mut bucket = self.bucket()?;
        // No-op rename is fine — just return the existing record.
        if old_name == new_name {
            return self
                .get_token_by_name_using(&mut bucket, owner_id, old_name)?
                .ok_or(AuthError::NotFound);
        }
        // New name must not already be in use by this owner.
        if bucket
            .get(&format!("token:by-name:{owner_id}:{new_name}"))?
            .is_some()
        {
            return Err(AuthError::NameTaken(new_name.to_string()));
        }
        // Find the existing record.
        let Some(bytes) = bucket.get(&format!("token:by-name:{owner_id}:{old_name}"))? else {
            return Err(AuthError::NotFound);
        };
        let token_id = String::from_utf8(bytes)
            .map_err(|_| AuthError::Malformed("token:by-name value".into()))?;
        let Some(mut token) = self.read_token_by_id(&mut bucket, &token_id)? else {
            return Err(AuthError::NotFound);
        };
        // Rewrite the name field on the record + swap the by-name index.
        // Order: set new index first, then update the record's name
        // field, then drop the old index. A crash mid-sequence leaves
        // an extra index entry pointing at the (renamed) record rather
        // than a dangling lookup.
        bucket.set(
            &format!("token:by-name:{owner_id}:{new_name}"),
            token.id.as_bytes().to_vec(),
        )?;
        bucket.hash_set(
            &format!("token:{}", token.id),
            "name",
            new_name.as_bytes().to_vec(),
        )?;
        bucket.delete(&format!("token:by-name:{owner_id}:{old_name}"))?;
        token.name = new_name.to_string();
        Ok(token)
    }

    fn get_token_by_name_using(
        &self,
        bucket: &mut Bucket,
        owner_id: &str,
        name: &str,
    ) -> Result<Option<Token>, AuthError> {
        let Some(bytes) = bucket.get(&format!("token:by-name:{owner_id}:{name}"))? else {
            return Ok(None);
        };
        let token_id = String::from_utf8(bytes)
            .map_err(|_| AuthError::Malformed("token:by-name value".into()))?;
        self.read_token_by_id(bucket, &token_id)
    }

    pub fn revoke_token_by_name(&self, owner_id: &str, name: &str) -> Result<bool, AuthError> {
        let mut bucket = self.bucket()?;
        let Some(bytes) = bucket.get(&format!("token:by-name:{owner_id}:{name}"))? else {
            return Ok(false);
        };
        let token_id = String::from_utf8(bytes)
            .map_err(|_| AuthError::Malformed("token:by-name value".into()))?;
        let Some(token) = self.read_token_by_id(&mut bucket, &token_id)? else {
            return Ok(false);
        };
        self.delete_token(&mut bucket, &token)?;
        Ok(true)
    }

    fn read_token_by_id(&self, bucket: &mut Bucket, id: &str) -> Result<Option<Token>, AuthError> {
        let fields = bucket.hash_get_all(&format!("token:{id}"))?;
        if fields.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_token(&fields)?))
    }

    fn delete_token(&self, bucket: &mut Bucket, token: &Token) -> Result<(), AuthError> {
        bucket.delete(&format!("token:by-hash:{}", token.hash))?;
        bucket.delete(&format!("token:by-name:{}:{}", token.owner_id, token.name))?;
        bucket.set_remove(&format!("token:by-owner:{}", token.owner_id), &token.id)?;
        for field in [
            "id",
            "name",
            "owner_id",
            "hash",
            "created_at",
            "expires_at",
            "last_used_at",
            "scopes",
        ] {
            bucket.hash_delete(&format!("token:{}", token.id), field)?;
        }
        Ok(())
    }

    // -- Authenticate -----------------------------------------------------

    /// Resolve a plaintext token to an `AuthContext`. Returns `Ok(None)`
    /// if the token doesn't exist or has expired.
    ///
    /// Two prefix families are accepted:
    /// * `wmt_<random>` — user-minted, long-lived; resolves via
    ///   `token:by-hash:{sha256}` to the `Token` record.
    /// * `wmm_<random>` — OAuth-flow-minted (ADR-0019); resolves via
    ///   `oauth:access:{sha256}` to the `mcp_oauth::AccessToken` record.
    ///   Both look up to the same `User` shape downstream.
    #[tracing::instrument(name = "auth.authenticate", skip_all)]
    pub fn authenticate(&self, plaintext: &str) -> Result<Option<AuthContext>, AuthError> {
        if plaintext.starts_with("wmt_") {
            return self.authenticate_wmt(plaintext);
        }
        if plaintext.starts_with("wmm_") {
            return self.authenticate_wmm(plaintext);
        }
        Ok(None)
    }

    fn authenticate_wmt(&self, plaintext: &str) -> Result<Option<AuthContext>, AuthError> {
        let mut bucket = self.bucket()?;
        let hash = sha256_hex(plaintext);
        let Some(token_id_bytes) = bucket.get(&format!("token:by-hash:{hash}"))? else {
            return Ok(None);
        };
        let token_id = String::from_utf8(token_id_bytes)
            .map_err(|_| AuthError::Malformed("token:by-hash value".into()))?;
        let Some(token) = self.read_token_by_id(&mut bucket, &token_id)? else {
            return Ok(None);
        };
        if let Some(expires) = token.expires_at
            && expires < Utc::now()
        {
            return Ok(None);
        }
        let Some(user) = self.read_user_by_id(&mut bucket, &token.owner_id)? else {
            return Ok(None);
        };
        // Best-effort touch of last_used_at; a failure here is logged but
        // doesn't block the auth result.
        let _ = bucket.hash_set(
            &format!("token:{}", token.id),
            "last_used_at",
            Utc::now().to_rfc3339().into_bytes(),
        );
        Ok(Some(AuthContext {
            user_id: user.id,
            user_name: user.name,
            is_admin: user.is_admin,
            credential_kind: CredentialKind::Token,
            credential_id: token.id,
        }))
    }

    fn authenticate_wmm(&self, plaintext: &str) -> Result<Option<AuthContext>, AuthError> {
        let mut bucket = self.bucket()?;
        let hash = sha256_hex(plaintext);
        let access = match crate::mcp_oauth::load_access_token(&mut bucket, &hash) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(None),
            Err(crate::mcp_oauth::OAuthStoreError::Storage(e)) => return Err(AuthError::from(e)),
            Err(crate::mcp_oauth::OAuthStoreError::Malformed(s)) => {
                return Err(AuthError::Malformed(s));
            }
            Err(crate::mcp_oauth::OAuthStoreError::NotFound) => return Ok(None),
        };
        if access.expires_at < Utc::now() {
            // The Valkey TTL should make this branch unreachable on
            // a healthy backend, but the in-memory store doesn't
            // expire records — explicit check keeps both behaviour-
            // identical.
            return Ok(None);
        }
        let Some(user) = self.read_user_by_id(&mut bucket, &access.user_id)? else {
            return Ok(None);
        };
        Ok(Some(AuthContext {
            user_id: user.id,
            user_name: user.name,
            is_admin: user.is_admin,
            credential_kind: CredentialKind::Token,
            credential_id: access.token_hash,
        }))
    }
}

// -- Encoding helpers ---------------------------------------------------------

fn generate_plaintext_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("wmt_{}", B64URL.encode(bytes))
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn decode_user(fields: &std::collections::HashMap<String, Vec<u8>>) -> Result<User, AuthError> {
    Ok(User {
        id: utf8(fields, "id")?,
        name: utf8(fields, "name")?,
        is_admin: utf8(fields, "is_admin")? == "1",
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
    })
}

fn decode_token(fields: &std::collections::HashMap<String, Vec<u8>>) -> Result<Token, AuthError> {
    let expires_at = match fields.get("expires_at") {
        None => None,
        Some(b) => {
            let s = std::str::from_utf8(b)
                .map_err(|_| AuthError::Malformed("token.expires_at not utf-8".into()))?;
            Some(parse_ts(s)?)
        }
    };
    let last_used_at = match fields.get("last_used_at") {
        None => None,
        Some(b) => {
            let s = std::str::from_utf8(b)
                .map_err(|_| AuthError::Malformed("token.last_used_at not utf-8".into()))?;
            Some(parse_ts(s)?)
        }
    };
    let scopes: Vec<String> = utf8(fields, "scopes")?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    Ok(Token {
        id: utf8(fields, "id")?,
        name: utf8(fields, "name")?,
        owner_id: utf8(fields, "owner_id")?,
        hash: utf8(fields, "hash")?,
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
        last_used_at,
        expires_at,
        scopes,
    })
}

fn utf8(
    fields: &std::collections::HashMap<String, Vec<u8>>,
    name: &str,
) -> Result<String, AuthError> {
    let bytes = fields
        .get(name)
        .ok_or_else(|| AuthError::Malformed(format!("field {name} missing")))?;
    String::from_utf8(bytes.clone())
        .map_err(|_| AuthError::Malformed(format!("field {name} not utf-8")))
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, AuthError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| AuthError::Malformed(format!("timestamp: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Auth {
        Auth::new(Storage::in_memory())
    }

    #[test]
    fn bootstrap_creates_admin_user_first_time() {
        let auth = fresh();
        let created = auth.bootstrap_admin("bootstrap", "wmt_test").unwrap();
        assert!(created);
        let user = auth
            .get_user_by_id(
                &auth
                    .read_user_by_name(&mut auth.bucket().unwrap(), "bootstrap")
                    .unwrap()
                    .unwrap()
                    .id,
            )
            .unwrap();
        assert!(user.is_admin);
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let auth = fresh();
        assert!(auth.bootstrap_admin("bootstrap", "wmt_a").unwrap());
        assert!(!auth.bootstrap_admin("bootstrap", "wmt_a").unwrap());
        // Idempotent in the sense of "don't re-create"; doesn't overwrite
        // the existing token even with a different plaintext.
        assert!(!auth.bootstrap_admin("bootstrap", "wmt_b").unwrap());
    }

    #[test]
    fn authenticate_with_correct_token() {
        let auth = fresh();
        auth.bootstrap_admin("bootstrap", "wmt_secret").unwrap();
        let ctx = auth.authenticate("wmt_secret").unwrap().unwrap();
        assert_eq!(ctx.user_name, "bootstrap");
        assert!(ctx.is_admin);
    }

    #[test]
    fn authenticate_with_wrong_token_returns_none() {
        let auth = fresh();
        auth.bootstrap_admin("bootstrap", "wmt_secret").unwrap();
        assert!(auth.authenticate("wmt_wrong").unwrap().is_none());
    }

    #[test]
    fn authenticate_rejects_non_wmt_prefix() {
        let auth = fresh();
        auth.bootstrap_admin("bootstrap", "wmt_secret").unwrap();
        assert!(auth.authenticate("Bearer wmt_secret").unwrap().is_none());
        assert!(auth.authenticate("plain-secret").unwrap().is_none());
    }

    #[test]
    fn create_and_revoke_token() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        let (token, plaintext) = auth.create_token(&user.id, "ci-runner", None).unwrap();
        assert!(plaintext.starts_with("wmt_"));
        assert_eq!(token.owner_id, user.id);
        // Token works.
        let ctx = auth.authenticate(&plaintext).unwrap().unwrap();
        assert_eq!(ctx.user_id, user.id);
        // Revoke and confirm it stops working.
        assert!(auth.revoke_token_by_name(&user.id, "ci-runner").unwrap());
        assert!(auth.authenticate(&plaintext).unwrap().is_none());
        // Idempotent: second revoke returns false.
        assert!(!auth.revoke_token_by_name(&user.id, "ci-runner").unwrap());
    }

    #[test]
    fn token_with_ttl_expires() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        // Negative TTL → already expired.
        let mut bucket = auth.bucket().unwrap();
        let token = Token {
            id: Ulid::new().to_string(),
            name: "expired".into(),
            owner_id: user.id.clone(),
            hash: sha256_hex("wmt_expired"),
            created_at: Utc::now() - Duration::hours(2),
            last_used_at: None,
            expires_at: Some(Utc::now() - Duration::hours(1)),
            scopes: default_scopes(),
        };
        // Manually write since create_token won't accept past expires_at.
        let key = format!("token:{}", token.id);
        bucket
            .hash_set(&key, "id", token.id.as_bytes().to_vec())
            .unwrap();
        bucket
            .hash_set(&key, "name", token.name.as_bytes().to_vec())
            .unwrap();
        bucket
            .hash_set(&key, "owner_id", token.owner_id.as_bytes().to_vec())
            .unwrap();
        bucket
            .hash_set(&key, "hash", token.hash.as_bytes().to_vec())
            .unwrap();
        bucket
            .hash_set(
                &key,
                "created_at",
                token.created_at.to_rfc3339().into_bytes(),
            )
            .unwrap();
        bucket
            .hash_set(
                &key,
                "expires_at",
                token.expires_at.unwrap().to_rfc3339().into_bytes(),
            )
            .unwrap();
        bucket
            .hash_set(&key, "scopes", token.scopes.join(" ").into_bytes())
            .unwrap();
        bucket
            .set(
                &format!("token:by-hash:{}", token.hash),
                token.id.as_bytes().to_vec(),
            )
            .unwrap();
        assert!(auth.authenticate("wmt_expired").unwrap().is_none());
    }

    #[test]
    fn list_tokens_returns_owners_tokens() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        auth.create_token(&user.id, "one", None).unwrap();
        auth.create_token(&user.id, "two", None).unwrap();
        let tokens = auth.list_tokens_for(&user.id).unwrap();
        // 2 created here + 1 from bootstrap = 3.
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn create_token_rejects_duplicate_name() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        auth.create_token(&user.id, "ci", None).unwrap();
        let err = auth.create_token(&user.id, "ci", None).unwrap_err();
        assert!(matches!(err, AuthError::NameTaken(_)));
    }

    #[test]
    fn rename_token_updates_record_and_index_and_keeps_plaintext_valid() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        let (token, plaintext) = auth.create_token(&user.id, "laptop-old", None).unwrap();
        let original_id = token.id.clone();
        let original_hash = token.hash.clone();

        let renamed = auth
            .rename_token(&user.id, "laptop-old", "laptop-new")
            .unwrap();
        assert_eq!(renamed.id, original_id, "id stable across rename");
        assert_eq!(renamed.hash, original_hash, "hash stable across rename");
        assert_eq!(renamed.name, "laptop-new");

        // Old name no longer resolves.
        assert!(
            auth.get_token_by_name(&user.id, "laptop-old")
                .unwrap()
                .is_none()
        );
        // New name does.
        let fetched = auth
            .get_token_by_name(&user.id, "laptop-new")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, original_id);
        // Plaintext token still authenticates.
        let ctx = auth.authenticate(&plaintext).unwrap();
        assert!(ctx.is_some(), "plaintext token still authenticates");
    }

    #[test]
    fn rename_token_rejects_name_collision() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        auth.create_token(&user.id, "ci", None).unwrap();
        auth.create_token(&user.id, "laptop", None).unwrap();
        let err = auth.rename_token(&user.id, "ci", "laptop").unwrap_err();
        assert!(matches!(err, AuthError::NameTaken(n) if n == "laptop"));
    }

    #[test]
    fn rename_token_to_same_name_is_a_noop() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        let (token, _) = auth.create_token(&user.id, "ci", None).unwrap();
        let renamed = auth.rename_token(&user.id, "ci", "ci").unwrap();
        assert_eq!(renamed.id, token.id);
        assert_eq!(renamed.name, "ci");
    }

    #[test]
    fn rename_token_returns_not_found_for_unknown_name() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth
            .read_user_by_name(&mut auth.bucket().unwrap(), "alice")
            .unwrap()
            .unwrap();
        let err = auth.rename_token(&user.id, "no-such", "new").unwrap_err();
        assert!(matches!(err, AuthError::NotFound));
    }

    #[test]
    fn list_users_returns_all_users() {
        let auth = fresh();
        auth.bootstrap_admin("bootstrap", "wmt_b").unwrap();
        auth.create_user("alice", false).unwrap();
        auth.create_user("bob", true).unwrap();
        let mut names: Vec<String> = auth
            .list_users()
            .unwrap()
            .into_iter()
            .map(|u| u.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["alice", "bob", "bootstrap"]);
    }

    #[test]
    fn count_admins_only_includes_admins() {
        let auth = fresh();
        auth.bootstrap_admin("bootstrap", "wmt_b").unwrap();
        auth.create_user("alice", false).unwrap();
        auth.create_user("bob", true).unwrap();
        assert_eq!(auth.count_admins().unwrap(), 2);
    }

    #[test]
    fn set_user_admin_toggles_flag() {
        let auth = fresh();
        let alice = auth.create_user("alice", false).unwrap();
        assert!(!alice.is_admin);
        let promoted = auth.set_user_admin(&alice.id, true).unwrap();
        assert!(promoted.is_admin);
        assert!(auth.get_user_by_id(&alice.id).unwrap().is_admin);
        let demoted = auth.set_user_admin(&alice.id, false).unwrap();
        assert!(!demoted.is_admin);
    }

    #[test]
    fn delete_user_cascades_tokens() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let alice = auth.get_user_by_name("alice").unwrap().unwrap();
        let (_t1, t1_plain) = auth.create_token(&alice.id, "extra", None).unwrap();
        // Both the bootstrap token and the extra token should authenticate.
        assert!(auth.authenticate("wmt_alice").unwrap().is_some());
        assert!(auth.authenticate(&t1_plain).unwrap().is_some());

        auth.delete_user(&alice.id).unwrap();

        // User is gone.
        assert!(auth.get_user_by_name("alice").unwrap().is_none());
        // All of the user's tokens stop authenticating.
        assert!(auth.authenticate("wmt_alice").unwrap().is_none());
        assert!(auth.authenticate(&t1_plain).unwrap().is_none());
    }

    #[test]
    fn new_token_gets_full_access_scopes() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        let user = auth.get_user_by_name("alice").unwrap().unwrap();
        let (token, _) = auth.create_token(&user.id, "ci", None).unwrap();
        assert_eq!(token.scopes, vec!["*".to_string()]);
        // Round-trip through storage preserves the value.
        let fetched = auth.get_token_by_name(&user.id, "ci").unwrap().unwrap();
        assert_eq!(fetched.scopes, vec!["*".to_string()]);
    }

    #[test]
    fn upsert_oauth_user_creates_then_returns_same_record() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "12345", "einarw", false)
            .unwrap();
        assert_eq!(first.name, "einarw");
        assert!(!first.is_admin);

        // Second call with the same identity returns the same user.
        let second = auth
            .upsert_oauth_user("github", "12345", "einarw", false)
            .unwrap();
        assert_eq!(second.id, first.id);
    }

    #[test]
    fn upsert_oauth_user_syncs_admin_flag() {
        let auth = fresh();
        let u = auth
            .upsert_oauth_user("github", "1", "alice", false)
            .unwrap();
        assert!(!u.is_admin);
        let u = auth
            .upsert_oauth_user("github", "1", "alice", true)
            .unwrap();
        assert!(u.is_admin);
        // Persisted.
        assert!(auth.get_user_by_id(&u.id).unwrap().is_admin);
        // And demote works.
        let u = auth
            .upsert_oauth_user("github", "1", "alice", false)
            .unwrap();
        assert!(!u.is_admin);
    }

    #[test]
    fn upsert_oauth_user_errors_on_name_collision() {
        let auth = fresh();
        auth.bootstrap_admin("alice", "wmt_alice").unwrap();
        // A fresh github identity using the same login as an existing
        // user must not silently take over that record — operator
        // resolves manually.
        let err = auth
            .upsert_oauth_user("github", "999", "alice", false)
            .unwrap_err();
        assert!(matches!(err, AuthError::NameTaken(n) if n == "alice"));
    }

    #[test]
    fn delete_user_unknown_id_is_not_found() {
        let auth = fresh();
        let err = auth
            .delete_user("01HZZZZZZZZZZZZZZZZZZZZZZZZZ")
            .unwrap_err();
        assert!(matches!(err, AuthError::NotFound));
    }
}
