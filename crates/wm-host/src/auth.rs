//! Authentication via bearer tokens.
//!
//! Storage layout per `storage-model.md`:
//!
//!   user:{ulid}                          hash with user fields
//!   user:by-email:{email}                string -> user ulid (THE unique
//!                                        identifier index; self-healing for
//!                                        pre-index records)
//!   user:all                             set of all user ulids
//!
//! Identity is email-only: `user:by-name` is retired (a legacy record's
//! stored `name` doubles as its identifier until a bootstrap/login
//! backfills the real email — see `decode_user`).
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
use rand::Rng;
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
    #[error("email already in use: {0}")]
    EmailTaken(String),
    #[error("malformed record in storage: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    /// The account identifier and cross-provider join key (per
    /// user-model.md): a *verified* email. Set at provisioning and
    /// never auto-updated afterwards — email rotation at a provider
    /// must not silently re-wire identity linking. The auth layer
    /// treats it as an opaque case-normalized unique string; email
    /// shape is enforced at the input boundaries (API, env vars,
    /// OAuth callbacks). Records that predate email-only identity
    /// decode their stored legacy `name` handle into this field until
    /// a bootstrap/login backfills the real email.
    pub email: String,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
    /// Monotonic counter backing "sign out everywhere". Every session
    /// is stamped with the value current at its creation; validation
    /// rejects a session whose stamp is behind this. Bumping it
    /// invalidates every session at once without enumerating any of
    /// them — see `auth-and-authz.md` "Sign out everywhere". Records
    /// written before the field existed decode as 0, which matches a
    /// freshly created user, so existing sessions stay valid.
    pub session_epoch: u64,
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
    pub user_email: String,
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

    /// Idempotently ensure an admin user exists with `email`, owning a
    /// token derived from the supplied plaintext. Returns `true` if a
    /// new user was created, `false` if the user already existed.
    pub fn bootstrap_admin(&self, email: &str, plaintext: &str) -> Result<bool, AuthError> {
        let mut bucket = self.bucket()?;
        let email = normalize_email(email);
        if self.find_user_by_email(&mut bucket, &email)?.is_some() {
            return Ok(false);
        }
        // Adopt a legacy pre-email bootstrap record (reachable via the
        // retired `user:by-name` index) instead of minting a second
        // admin: its existing token keeps working and the record gains
        // the email as its identifier.
        if let Some(bytes) = bucket.get("user:by-name:bootstrap")? {
            let id = String::from_utf8(bytes)
                .map_err(|_| AuthError::Malformed("user:by-name value".into()))?;
            if let Some(user) = self.read_user_by_id(&mut bucket, &id)? {
                let fields = bucket.hash_get_all(&format!("user:{}", user.id))?;
                if !fields.contains_key("primary_email") {
                    bucket.hash_set(
                        &format!("user:{}", user.id),
                        "primary_email",
                        email.as_bytes().to_vec(),
                    )?;
                    bucket.set(
                        &format!("user:by-email:{email}"),
                        user.id.as_bytes().to_vec(),
                    )?;
                    bucket.delete("user:by-name:bootstrap")?;
                    return Ok(false);
                }
            }
        }
        let user = match self.write_new_user(&mut bucket, &email, true) {
            Ok(u) => u,
            // Lost the atomic email claim to a sibling replica cold-
            // starting at the same instant (ADR-0037 item 6). That is
            // the already-exists case arriving a few milliseconds late,
            // not a failure: propagating it would abort startup and
            // crash-loop every replica but the winner, when the whole
            // point of the guard is that simultaneous cold starts
            // converge on one record. Only this error — anything else
            // is a real problem and still aborts.
            Err(AuthError::EmailTaken(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
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

    pub fn create_user(&self, email: &str, is_admin: bool) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        self.write_new_user(&mut bucket, &normalize_email(email), is_admin)
    }

    pub fn get_user_by_id(&self, id: &str) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        self.read_user_by_id(&mut bucket, id)?
            .ok_or(AuthError::NotFound)
    }

    /// Resolve a user by email — the admin-surface selector. Legacy
    /// records that predate email-only identity are still reachable by
    /// their old name handle (their decoded `email` IS that handle, so
    /// the scan fallback matches it).
    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>, AuthError> {
        let mut bucket = self.bucket()?;
        self.find_user_by_email(&mut bucket, &normalize_email(email))
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
    /// env var"). The email is the user record's identifier and also
    /// the `subject` on the implicit `local:` identity index — so an
    /// OAuth-provisioned account with the same email links here too,
    /// same contract as `upsert_oauth_user`.
    pub fn upsert_local_user(&self, email: &str, is_admin: bool) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        let email = normalize_email(email);
        if let Some(mut user) = self.find_user_by_email(&mut bucket, &email)? {
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
                &format!("user:by-identity:local:{email}"),
                user.id.as_bytes().to_vec(),
            )?;
            return Ok(user);
        }
        let user = self.write_new_user(&mut bucket, &email, is_admin)?;
        bucket.set(
            &format!("user:by-identity:local:{email}"),
            user.id.as_bytes().to_vec(),
        )?;
        Ok(user)
    }

    /// Upsert a user keyed on an external identity (e.g. a GitHub
    /// numeric ID or an OIDC `sub`). Returning users are looked up via
    /// `user:by-identity:{provider}:{subject}` so the account survives
    /// renames on the provider side.
    ///
    /// `email` is the provider's **verified** email claim — callers
    /// must refuse the login before getting here when the provider
    /// supplied none (linking on an unverified claim would be an
    /// account-takeover vector, and accounts are keyed by email). It's
    /// the cross-provider join key per user-model.md: a first-seen
    /// `(provider, subject)` whose verified email matches an existing
    /// user's email is **linked** to that user, so the same human
    /// logging in via GitHub and via an OIDC IdP lands in one account,
    /// in either order.
    pub fn upsert_oauth_user(
        &self,
        provider: &str,
        subject: &str,
        email: &str,
        is_admin: bool,
    ) -> Result<User, AuthError> {
        let mut bucket = self.bucket()?;
        let identity_key = format!("user:by-identity:{provider}:{subject}");
        let email = normalize_email(email);

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
                // Records that predate email-only identity decode
                // their legacy name handle as `email`; backfill the
                // real one. Set-once, not an update — a stored email
                // never moves (user-model.md: email rotation must not
                // silently re-wire linking).
                let fields = bucket.hash_get_all(&format!("user:{}", user.id))?;
                if !fields.contains_key("primary_email") {
                    if self.find_user_by_email(&mut bucket, &email)?.is_none() {
                        bucket.hash_set(
                            &format!("user:{}", user.id),
                            "primary_email",
                            email.as_bytes().to_vec(),
                        )?;
                        bucket.set(
                            &format!("user:by-email:{email}"),
                            user.id.as_bytes().to_vec(),
                        )?;
                        user.email = email;
                    }
                } else {
                    // Self-heal the by-email index for records written
                    // before it existed (idempotent set).
                    bucket.set(
                        &format!("user:by-email:{}", user.email),
                        user.id.as_bytes().to_vec(),
                    )?;
                }
                return Ok(user);
            }
            // Stale identity index pointing at a deleted user record.
            // Drop the stale entry and fall through to create.
            bucket.delete(&identity_key)?;
        }

        // First-seen identity. Cross-provider linking (user-model.md):
        // if a user already holds this verified email, this is the
        // same person arriving via a new provider — attach the
        // identity to them. A linear scan over user:all is the
        // doc-blessed lookup at this deployment scale.
        if let Some(mut user) = self.find_user_by_email(&mut bucket, &email)? {
            bucket.set(&identity_key, user.id.as_bytes().to_vec())?;
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

        let user = self.write_new_user(&mut bucket, &email, is_admin)?;
        bucket.set(&identity_key, user.id.as_bytes().to_vec())?;
        Ok(user)
    }

    /// Look up a user by email. Index-first (`user:by-email:{email}`);
    /// on a miss, falls back to a scan and **self-heals** the index —
    /// records written before the index existed get their entry
    /// created the first time anything looks them up by email. The
    /// scan also matches legacy records by their old name handle
    /// (their decoded `email` IS that handle); those are never
    /// indexed — only real emails go in the index.
    fn find_user_by_email(
        &self,
        bucket: &mut Bucket,
        email: &str,
    ) -> Result<Option<User>, AuthError> {
        if let Some(bytes) = bucket.get(&format!("user:by-email:{email}"))? {
            let id = String::from_utf8(bytes)
                .map_err(|_| AuthError::Malformed("user:by-email value".into()))?;
            if let Some(user) = self.read_user_by_id(bucket, &id)?
                && user.email == email
            {
                return Ok(Some(user));
            }
            // Stale index entry (user deleted out-of-band, or migrated
            // to a different identifier): drop it and fall through to
            // the scan.
            bucket.delete(&format!("user:by-email:{email}"))?;
        }
        for id in bucket.set_members("user:all")? {
            if let Some(user) = self.read_user_by_id(bucket, &id)?
                && user.email == email
            {
                if email.contains('@') {
                    bucket.set(
                        &format!("user:by-email:{email}"),
                        user.id.as_bytes().to_vec(),
                    )?;
                }
                return Ok(Some(user));
            }
        }
        Ok(None)
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

    /// Invalidate every session belonging to this user by bumping
    /// their session epoch. Returns the new value. Nothing is
    /// enumerated: live sessions carry the old stamp and fail the
    /// comparison in the auth path on their next request.
    ///
    /// Tokens are unaffected — they are a separate credential with
    /// their own revocation (`revoke_token_by_name`).
    pub fn bump_session_epoch(&self, id: &str) -> Result<u64, AuthError> {
        let mut bucket = self.bucket()?;
        // Read first so a missing user is a NotFound rather than a
        // hash_incr silently creating one.
        self.read_user_by_id(&mut bucket, id)?
            .ok_or(AuthError::NotFound)?;
        let next = bucket.hash_incr(&format!("user:{id}"), "session_epoch", 1)?;
        Ok(next as u64)
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

        bucket.delete(&format!("user:by-email:{}", user.email))?;
        // Legacy records may still carry a retired user:by-name entry
        // (their decoded email doubles as that handle).
        bucket.delete(&format!("user:by-name:{}", user.email))?;
        bucket.set_remove("user:all", &user.id)?;
        for field in ["id", "name", "primary_email", "is_admin", "created_at"] {
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

    fn write_new_user(
        &self,
        bucket: &mut Bucket,
        email: &str,
        is_admin: bool,
    ) -> Result<User, AuthError> {
        // Emails are THE identifier, so the index must stay unique.
        // Linking lookups usually run first; this guards racing logins
        // and explicit creation.
        if self.find_user_by_email(bucket, email)?.is_some() {
            return Err(AuthError::EmailTaken(email.to_string()));
        }
        let user = User {
            id: Ulid::generate().to_string(),
            email: email.to_string(),
            is_admin,
            created_at: Utc::now(),
            session_epoch: 0,
        };
        let key = format!("user:{}", user.id);
        bucket.hash_set(&key, "id", user.id.as_bytes().to_vec())?;
        // Storage field name kept from the pre-email-only era so
        // existing records need no migration.
        bucket.hash_set(&key, "primary_email", user.email.as_bytes().to_vec())?;
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
        bucket.hash_set(&key, "session_epoch", b"0".to_vec())?;
        // Claim the email index atomically (ADR-0037 item 6). The
        // check at the top of this function is check-then-act, which
        // two replicas cold-starting against an empty store — or two
        // simultaneous first-logins for the same OIDC identity — can
        // both pass. Losing this claim means someone else created the
        // account a moment ago, which is exactly `EmailTaken`.
        //
        // Written after the record so the index never points at a
        // half-built user; the loser's orphaned `user:{id}` hash is
        // unreachable (nothing indexes it) and harmless.
        let claimed = bucket.set_if_absent(
            &format!("user:by-email:{}", user.email),
            user.id.as_bytes().to_vec(),
        )?;
        if !claimed {
            return Err(AuthError::EmailTaken(user.email.clone()));
        }
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
            id: Ulid::generate().to_string(),
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
            user_email: user.email,
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
            user_email: user.email,
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

/// Canonical form for email comparison and storage: trimmed +
/// ASCII-lowercased. Emails are compared as opaque case-insensitive
/// strings — no plus-suffix stripping or unicode normalization games,
/// which would *widen* the linking match and weaken it.
fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn decode_user(fields: &std::collections::HashMap<String, Vec<u8>>) -> Result<User, AuthError> {
    // `primary_email` is the stored identifier (field name predates
    // email-only identity). Records written before it existed carry
    // only `name`; that legacy handle serves as the identifier until
    // a bootstrap/login backfills the real email.
    let email = match fields.get("primary_email") {
        Some(b) => String::from_utf8(b.clone())
            .map_err(|_| AuthError::Malformed("user.primary_email not utf-8".into()))?,
        None => utf8(fields, "name")?,
    };
    // Absent on records written before "sign out everywhere" existed.
    // 0 is the same value a new user starts at, so their live sessions
    // (also stamped 0 by the same default) keep working.
    let session_epoch = match fields.get("session_epoch") {
        Some(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AuthError::Malformed("user.session_epoch not a u64".into()))?,
        None => 0,
    };
    Ok(User {
        id: utf8(fields, "id")?,
        email,
        is_admin: utf8(fields, "is_admin")? == "1",
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
        session_epoch,
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

    /// Write a user record the way the pre-email-only code did: a
    /// `name` field, no `primary_email`, a `user:by-name` index entry.
    /// Used to prove the migration/adoption paths.
    fn write_legacy_named_user(auth: &Auth, name: &str, is_admin: bool) -> String {
        let mut bucket = auth.bucket().unwrap();
        let id = Ulid::generate().to_string();
        let key = format!("user:{id}");
        bucket.hash_set(&key, "id", id.as_bytes().to_vec()).unwrap();
        bucket
            .hash_set(&key, "name", name.as_bytes().to_vec())
            .unwrap();
        bucket
            .hash_set(
                &key,
                "is_admin",
                if is_admin { b"1" } else { b"0" }.to_vec(),
            )
            .unwrap();
        bucket
            .hash_set(&key, "created_at", Utc::now().to_rfc3339().into_bytes())
            .unwrap();
        bucket
            .set(&format!("user:by-name:{name}"), id.as_bytes().to_vec())
            .unwrap();
        bucket.set_add("user:all", &id).unwrap();
        id
    }

    #[test]
    fn bootstrap_creates_admin_user_first_time() {
        let auth = fresh();
        let created = auth
            .bootstrap_admin("Root@Test.Example", "wmt_test")
            .unwrap();
        assert!(created);
        let user = auth
            .get_user_by_email("root@test.example")
            .unwrap()
            .unwrap();
        assert!(user.is_admin);
        assert_eq!(user.email, "root@test.example", "email is normalized");
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let auth = fresh();
        assert!(auth.bootstrap_admin("root@test.example", "wmt_a").unwrap());
        assert!(!auth.bootstrap_admin("root@test.example", "wmt_a").unwrap());
        // Idempotent in the sense of "don't re-create"; doesn't overwrite
        // the existing token even with a different plaintext.
        assert!(!auth.bootstrap_admin("root@test.example", "wmt_b").unwrap());
    }

    #[test]
    fn bootstrap_adopts_legacy_named_record() {
        let auth = fresh();
        // A deployment from before email-only identity: the bootstrap
        // admin exists under the name handle, no email.
        let legacy_id = write_legacy_named_user(&auth, "bootstrap", true);
        // Restart with WM_BOOTSTRAP_EMAIL set: the record is adopted,
        // not duplicated (and its existing token would keep working —
        // no new token is minted).
        let created = auth
            .bootstrap_admin("root@test.example", "wmt_test")
            .unwrap();
        assert!(!created, "adoption, not creation");
        assert_eq!(auth.list_users().unwrap().len(), 1);
        let user = auth
            .get_user_by_email("root@test.example")
            .unwrap()
            .unwrap();
        assert_eq!(user.id, legacy_id);
        // The old handle no longer addresses it — the identifier moved.
        assert!(auth.get_user_by_email("bootstrap").unwrap().is_none());
    }

    #[test]
    fn authenticate_with_correct_token() {
        let auth = fresh();
        auth.bootstrap_admin("root@test.example", "wmt_secret")
            .unwrap();
        let ctx = auth.authenticate("wmt_secret").unwrap().unwrap();
        assert_eq!(ctx.user_email, "root@test.example");
        assert!(ctx.is_admin);
    }

    #[test]
    fn authenticate_with_wrong_token_returns_none() {
        let auth = fresh();
        auth.bootstrap_admin("root@test.example", "wmt_secret")
            .unwrap();
        assert!(auth.authenticate("wmt_wrong").unwrap().is_none());
    }

    #[test]
    fn authenticate_rejects_non_wmt_prefix() {
        let auth = fresh();
        auth.bootstrap_admin("root@test.example", "wmt_secret")
            .unwrap();
        assert!(auth.authenticate("Bearer wmt_secret").unwrap().is_none());
        assert!(auth.authenticate("plain-secret").unwrap().is_none());
    }

    #[test]
    fn create_and_revoke_token() {
        let auth = fresh();
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .unwrap();
        // Negative TTL → already expired.
        let mut bucket = auth.bucket().unwrap();
        let token = Token {
            id: Ulid::generate().to_string(),
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .unwrap();
        auth.create_token(&user.id, "ci", None).unwrap();
        let err = auth.create_token(&user.id, "ci", None).unwrap_err();
        assert!(matches!(err, AuthError::NameTaken(_)));
    }

    #[test]
    fn rename_token_updates_record_and_index_and_keeps_plaintext_valid() {
        let auth = fresh();
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .unwrap();
        let err = auth.rename_token(&user.id, "no-such", "new").unwrap_err();
        assert!(matches!(err, AuthError::NotFound));
    }

    #[test]
    fn list_users_returns_all_users() {
        let auth = fresh();
        auth.bootstrap_admin("root@test.example", "wmt_b").unwrap();
        auth.create_user("alice@test.example", false).unwrap();
        auth.create_user("bob@test.example", true).unwrap();
        let mut emails: Vec<String> = auth
            .list_users()
            .unwrap()
            .into_iter()
            .map(|u| u.email)
            .collect();
        emails.sort();
        assert_eq!(
            emails,
            vec![
                "alice@test.example",
                "bob@test.example",
                "root@test.example"
            ]
        );
    }

    #[test]
    fn count_admins_only_includes_admins() {
        let auth = fresh();
        auth.bootstrap_admin("root@test.example", "wmt_b").unwrap();
        auth.create_user("alice@test.example", false).unwrap();
        auth.create_user("bob@test.example", true).unwrap();
        assert_eq!(auth.count_admins().unwrap(), 2);
    }

    #[test]
    fn simultaneous_bootstrap_converges_on_one_admin() {
        // Two replicas cold-starting against the same empty storage.
        // The atomic email claim means one loses — and losing must be
        // "the admin already exists", not a startup abort, or every
        // replica but the winner crash-loops on first install.
        let storage = Storage::in_memory();
        let a = Auth::new(storage.clone());
        let b = Auth::new(storage);

        let first = a.bootstrap_admin("admin@test.example", "wmt_a").unwrap();
        let second = b.bootstrap_admin("admin@test.example", "wmt_b").unwrap();

        assert!(first, "the winner created the admin");
        assert!(
            !second,
            "the loser reports already-exists rather than erroring"
        );
        assert_eq!(a.list_users().unwrap().len(), 1, "exactly one admin record");
    }

    #[test]
    fn set_user_admin_toggles_flag() {
        let auth = fresh();
        let alice = auth.create_user("alice@test.example", false).unwrap();
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
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let alice = auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .unwrap();
        let (_t1, t1_plain) = auth.create_token(&alice.id, "extra", None).unwrap();
        // Both the bootstrap token and the extra token should authenticate.
        assert!(auth.authenticate("wmt_alice").unwrap().is_some());
        assert!(auth.authenticate(&t1_plain).unwrap().is_some());

        auth.delete_user(&alice.id).unwrap();

        // User is gone.
        assert!(
            auth.get_user_by_email("alice@test.example")
                .unwrap()
                .is_none()
        );
        // All of the user's tokens stop authenticating.
        assert!(auth.authenticate("wmt_alice").unwrap().is_none());
        assert!(auth.authenticate(&t1_plain).unwrap().is_none());
    }

    #[test]
    fn new_token_gets_full_access_scopes() {
        let auth = fresh();
        auth.bootstrap_admin("alice@test.example", "wmt_alice")
            .unwrap();
        let user = auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .unwrap();
        let (token, _) = auth.create_token(&user.id, "ci", None).unwrap();
        assert_eq!(token.scopes, vec!["*".to_string()]);
        // Round-trip through storage preserves the value.
        let fetched = auth.get_token_by_name(&user.id, "ci").unwrap().unwrap();
        assert_eq!(fetched.scopes, vec!["*".to_string()]);
    }

    #[test]
    fn create_user_rejects_duplicate_email() {
        let auth = fresh();
        auth.create_user("alice@test.example", false).unwrap();
        let err = auth.create_user("alice@test.example", true).unwrap_err();
        assert!(matches!(err, AuthError::EmailTaken(_)));
        // Case-insensitive: same address, different casing.
        let err = auth.create_user("Alice@Test.Example", false).unwrap_err();
        assert!(matches!(err, AuthError::EmailTaken(_)));
    }

    #[test]
    fn upsert_oauth_user_creates_then_returns_same_record() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "12345", "dev@acme.example", false)
            .unwrap();
        assert_eq!(first.email, "dev@acme.example");
        assert!(!first.is_admin);

        // Second call with the same identity returns the same user.
        let second = auth
            .upsert_oauth_user("github", "12345", "dev@acme.example", false)
            .unwrap();
        assert_eq!(second.id, first.id);
    }

    #[test]
    fn upsert_oauth_user_syncs_admin_flag() {
        let auth = fresh();
        let u = auth
            .upsert_oauth_user("github", "1", "alice@test.example", false)
            .unwrap();
        assert!(!u.is_admin);
        let u = auth
            .upsert_oauth_user("github", "1", "alice@test.example", true)
            .unwrap();
        assert!(u.is_admin);
        // Persisted.
        assert!(auth.get_user_by_id(&u.id).unwrap().is_admin);
        // And demote works.
        let u = auth
            .upsert_oauth_user("github", "1", "alice@test.example", false)
            .unwrap();
        assert!(!u.is_admin);
    }

    #[test]
    fn oauth_user_links_across_providers_by_verified_email() {
        let auth = fresh();
        // Same human: GitHub first, OIDC second, same verified email.
        let via_github = auth
            .upsert_oauth_user("github", "12345", "dev@acme.example", false)
            .unwrap();
        let via_oidc = auth
            .upsert_oauth_user("oidc", "sub-abc", "dev@acme.example", false)
            .unwrap();
        assert_eq!(via_oidc.id, via_github.id, "one account, two providers");
        // Both identities now resolve to the same user.
        let again = auth
            .upsert_oauth_user("oidc", "sub-abc", "dev@acme.example", false)
            .unwrap();
        assert_eq!(again.id, via_github.id);
    }

    #[test]
    fn oauth_email_linking_is_case_insensitive_and_trimmed() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "1", "Alice@Corp.Example", false)
            .unwrap();
        assert_eq!(first.email, "alice@corp.example");
        let linked = auth
            .upsert_oauth_user("oidc", "s1", "  alice@corp.example ", false)
            .unwrap();
        assert_eq!(linked.id, first.id);
    }

    #[test]
    fn oauth_users_with_different_emails_are_distinct_accounts() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "1", "alice@corp.example", false)
            .unwrap();
        // Different verified email = genuinely different person: a
        // second account, never a merge.
        let second = auth
            .upsert_oauth_user("oidc", "s1", "alice@other.example", false)
            .unwrap();
        assert_ne!(second.id, first.id);
        // And both are addressable by their emails.
        assert_eq!(
            auth.get_user_by_email("alice@corp.example")
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        assert_eq!(
            auth.get_user_by_email("alice@other.example")
                .unwrap()
                .unwrap()
                .id,
            second.id
        );
    }

    #[test]
    fn returning_identity_keeps_stored_email() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "1", "alice@corp.example", false)
            .unwrap();
        // A later login with a DIFFERENT email must not move the
        // identifier — set-once (user-model.md).
        let unchanged = auth
            .upsert_oauth_user("github", "1", "new@corp.example", false)
            .unwrap();
        assert_eq!(unchanged.id, first.id);
        assert_eq!(unchanged.email, "alice@corp.example");
        // And no phantom account under the new email.
        assert!(
            auth.get_user_by_email("new@corp.example")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_record_backfills_email_on_login() {
        let auth = fresh();
        // Record + identity index from before email-only identity.
        let legacy_id = write_legacy_named_user(&auth, "einarw", false);
        auth.bucket()
            .unwrap()
            .set("user:by-identity:github:1", legacy_id.as_bytes().to_vec())
            .unwrap();
        // Next login supplies the verified email: backfilled in place.
        let user = auth
            .upsert_oauth_user("github", "1", "dev@acme.example", false)
            .unwrap();
        assert_eq!(user.id, legacy_id);
        assert_eq!(user.email, "dev@acme.example");
        assert_eq!(
            auth.get_user_by_email("dev@acme.example")
                .unwrap()
                .unwrap()
                .id,
            legacy_id
        );
    }

    #[test]
    fn legacy_record_still_addressable_by_old_handle() {
        let auth = fresh();
        let legacy_id = write_legacy_named_user(&auth, "bootstrap", true);
        // Until a bootstrap/login backfills the email, the old handle
        // works as the selector (scan fallback)...
        let user = auth.get_user_by_email("bootstrap").unwrap().unwrap();
        assert_eq!(user.id, legacy_id);
        // ...but a bare handle is never written into the email index.
        assert!(
            auth.bucket()
                .unwrap()
                .get("user:by-email:bootstrap")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn linking_syncs_admin_flag_from_the_linking_provider() {
        let auth = fresh();
        let first = auth
            .upsert_oauth_user("github", "1", "alice@corp.example", false)
            .unwrap();
        assert!(!first.is_admin);
        // The OIDC provider's rules say admin: the linked account is
        // promoted, same contract as the returning-identity path.
        let linked = auth
            .upsert_oauth_user("oidc", "s1", "alice@corp.example", true)
            .unwrap();
        assert_eq!(linked.id, first.id);
        assert!(linked.is_admin);
        assert!(auth.get_user_by_id(&first.id).unwrap().is_admin);
    }

    #[test]
    fn upsert_local_user_links_by_email() {
        let auth = fresh();
        // OAuth account first; local-auth entry with the same email
        // addresses the SAME account (one human, two login doors).
        let via_oauth = auth
            .upsert_oauth_user("github", "1", "alice@corp.example", false)
            .unwrap();
        let via_local = auth.upsert_local_user("alice@corp.example", true).unwrap();
        assert_eq!(via_local.id, via_oauth.id);
        assert!(via_local.is_admin, "role synced from the env var");
    }

    #[test]
    fn deleted_user_leaves_no_stale_email_link() {
        let auth = fresh();
        let user = auth
            .upsert_oauth_user("github", "1", "alice@corp.example", false)
            .unwrap();
        auth.delete_user(&user.id).unwrap();
        // A new identity re-using the email starts fresh.
        let fresh_user = auth
            .upsert_oauth_user("oidc", "s2", "alice@corp.example", false)
            .unwrap();
        assert_ne!(fresh_user.id, user.id);
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
