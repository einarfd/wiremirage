//! Route + group registry. Records live in storage under the keys defined
//! by `storage-model.md`:
//!
//!   route:{ulid}                                hash with full route record
//!   route:in-group:{group_ulid}                 set of route ulids
//!   route:by-number:{group_ulid}:{n}            string -> route ulid
//!   route:by-method-path:{METHOD}:{path}        string -> route ulid (exact-match index)
//!   route:all                                   set of all route ulids
//!
//!   group:{ulid}                                hash with group record
//!   group:by-name:{name}                        string -> group ulid
//!   group:counters:{group_ulid}                 hash with next_route_number
//!
//! All storage access goes through `Storage::admin_bucket()` (a no-prefix
//! `Bucket`), so the same code path runs against in-memory and Valkey
//! backends.
//!
//! Slice 3 scope: implicit single-route groups, no TTL, no auth, no
//! cascade. Conflict detection beyond the by-method-path exact-match index
//! lands in a follow-up task in this slice.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use crate::pattern::{self, Methods, Pattern, PatternError};
use crate::store::{Bucket, Storage, StoreError};

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("storage backend error: {0}")]
    Storage(#[from] StoreError),
    #[error("route or group not found")]
    NotFound,
    #[error("route conflict: {0}")]
    Conflict(String),
    #[error("malformed record in storage: {0}")]
    Malformed(String),
    #[error("invalid path pattern: {0}")]
    InvalidPath(#[from] PatternError),
    #[error("invalid method `{0}`: must be uppercase ASCII or `ANY`")]
    InvalidMethod(String),
}

/// One stored route. Fields here mirror the REST `route record` shape from
/// `rest-api.md`, minus the auth/journal/timestamps surface that hasn't
/// landed yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub group_id: String,
    pub group_name: String,
    pub number: u32,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub bindings_version: String,
    #[serde(with = "serde_bytes")]
    pub compiled_wasm: Vec<u8>,
    /// Original handler source as submitted by the caller. `Some` for
    /// source-language routes (`typescript`, `javascript`, ...) so the
    /// UI / CLI / MCP can show the code; `None` for routes uploaded as
    /// pre-compiled `wasm` and for routes that pre-date this field.
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    /// User ULID of the caller that created the route. DELETE/PATCH require
    /// the caller to match this id (or to be admin).
    pub owner_id: String,
    /// Cumulative count of matched dispatches against this route. Bumped
    /// on every successful match; `0` for never-hit routes. Used as the
    /// `hits_total` field in REST responses and as a sort column on
    /// `GET /__api/routes`.
    pub hits_total: u64,
    /// Timestamp of the most recent matched dispatch. `None` for
    /// never-hit routes (or routes that pre-date this field). Used as
    /// the default sort column on `GET /__api/routes`.
    pub last_hit_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub implicit: bool,
    pub created_at: DateTime<Utc>,
    /// User ULID of the group's creator. Lifecycle operations
    /// (PATCH/DELETE/refresh) require the caller to match this id or
    /// to be an admin.
    pub owner_id: String,
    /// Configured TTL on the group record, in seconds. The actual
    /// remaining lifetime tracks via the Valkey `EXPIRE` set on the
    /// `group:{ulid}` hash key; this field is the *configured* value
    /// (used to compute fresh expiries on refresh / sliding bumps).
    pub ttl_seconds: u64,
    /// When `true`, the group's TTL is bumped to `ttl_seconds` on
    /// every successful route match in dispatch. Implicit groups
    /// default to `true` so they live as long as traffic flows;
    /// explicit groups default to `true` too but can opt out.
    pub sliding_ttl: bool,
    /// Timestamp of the most recent matched dispatch against any
    /// route in this group. `None` for groups that have never seen
    /// traffic. Used as the default sort column on `GET /__api/groups`.
    pub last_activity_at: Option<DateTime<Utc>>,
}

/// Default lifetime for a newly created group when the caller doesn't
/// supply `ttl_seconds`. Per `storage-model.md` "TTL defaults and
/// bounds".
pub const DEFAULT_GROUP_TTL_SECONDS: u64 = 24 * 60 * 60;
/// Hard upper bound on the configured TTL. Per the same table.
pub const MAX_GROUP_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
pub const DEFAULT_GROUP_SLIDING_TTL: bool = true;

#[derive(Debug, Clone)]
pub struct NewGroup {
    pub name: String,
    pub owner_id: String,
    /// `None` falls back to `DEFAULT_GROUP_TTL_SECONDS`.
    pub ttl_seconds: Option<u64>,
    /// `None` falls back to `DEFAULT_GROUP_SLIDING_TTL`.
    pub sliding_ttl: Option<bool>,
}

/// Parameters accepted from the REST handler when creating a route.
#[derive(Debug, Clone)]
pub struct NewRoute {
    /// Group name or ULID. `None` triggers creation of an implicit single-
    /// route group named `_route_{ulid_suffix}`.
    pub group: Option<String>,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub bindings_version: String,
    pub compiled_wasm: Vec<u8>,
    /// Original handler source for source-language routes. `None` for
    /// pre-compiled `wasm` uploads (no source ever existed in the host).
    pub source: Option<String>,
    /// User ULID of the caller creating this route.
    pub owner_id: String,
}

/// Partial-update payload for `update_route`. Every field is
/// `Option`; `Some` means "replace with this", `None` means "leave
/// alone". `language`/`bindings_version`/`compiled_wasm` move together
/// (an artifact swap), enforced at the API layer; the registry trusts
/// the caller to send a consistent triple.
#[derive(Debug, Clone, Default)]
pub struct PatchRoute {
    pub methods: Option<Vec<String>>,
    pub path: Option<String>,
    pub language: Option<String>,
    pub bindings_version: Option<String>,
    pub compiled_wasm: Option<Vec<u8>>,
    /// New source to persist alongside `compiled_wasm`. `None` means
    /// "leave alone"; `Some(None)` means "clear" (e.g. a wasm swap that
    /// has no source). The API layer enforces consistency — a
    /// source-language swap sends `Some(Some(src))`; a wasm swap sends
    /// `Some(None)`.
    pub source: Option<Option<String>>,
}

/// One entry from a route's per-route kv namespace. `kind` is the
/// storage-level type — `"bytes"`, `"list"`, `"hash"`, `"set"`, or
/// `"other"` (for a co-resident application's exotic Redis types).
/// `value` is `Some` only for `kind = "bytes"`; for collection kinds
/// `length` carries the element / field / member count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStateEntry {
    pub key: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// Slug rendering: `{group_name}/{number}`.
pub fn render_slug(group_name: &str, number: u32) -> String {
    format!("{group_name}/{number}")
}

pub struct Registry {
    storage: Storage,
}

impl Registry {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    fn bucket(&self) -> Result<Bucket, RegistryError> {
        Ok(self.storage.admin_bucket()?)
    }

    /// Test-only escape hatch for poking at storage directly (e.g.
    /// the lifecycle sweeper test suite manually wipes a group
    /// record to simulate Valkey TTL firing). Production code should
    /// route through the typed registry methods.
    #[cfg(test)]
    pub(crate) fn admin_bucket_for_test(&self) -> Bucket {
        self.storage.admin_bucket().expect("admin bucket")
    }

    // -- Group operations --------------------------------------------------

    /// Resolve a group reference (name or ULID) to its ULID, or `None` if
    /// it doesn't exist.
    fn resolve_group(
        &self,
        bucket: &mut Bucket,
        reference: &str,
    ) -> Result<Option<String>, RegistryError> {
        // ULID first: 26-char Crockford base32 with no leading underscore.
        if Ulid::from_string(reference).is_ok()
            && !reference.is_empty()
            && bucket
                .hash_get_all(&format!("group:{reference}"))?
                .contains_key("id")
        {
            return Ok(Some(reference.to_string()));
        }
        if let Some(bytes) = bucket.get(&format!("group:by-name:{reference}"))? {
            let s = String::from_utf8(bytes)
                .map_err(|_| RegistryError::Malformed("group:by-name value".into()))?;
            return Ok(Some(s));
        }
        Ok(None)
    }

    fn read_group(&self, bucket: &mut Bucket, group_id: &str) -> Result<Group, RegistryError> {
        let fields = bucket.hash_get_all(&format!("group:{group_id}"))?;
        if fields.is_empty() {
            return Err(RegistryError::NotFound);
        }
        decode_group(&fields)
    }

    /// Resolve a group reference (name or ULID) and return the group
    /// record, or `NotFound` if no such group exists. Used by
    /// `/__api/journal/{group}` so callers can refer to groups by
    /// either form.
    pub fn read_group_by_ref(&self, reference: &str) -> Result<Group, RegistryError> {
        let mut bucket = self.bucket()?;
        let id = self
            .resolve_group(&mut bucket, reference)?
            .ok_or(RegistryError::NotFound)?;
        self.read_group(&mut bucket, &id)
    }

    /// Explicit group creation. Validates name uniqueness, normalizes
    /// the TTL config, writes the record + indexes + Valkey TTL.
    pub fn create_group(&self, params: NewGroup) -> Result<Group, RegistryError> {
        let mut bucket = self.bucket()?;
        // A blank name means "assign me a friendly one" (ADR-0030): group
        // names double as subdomains, so the generated label is DNS-safe.
        // An explicit name is taken as-is, subject to the uniqueness check.
        let name = if params.name.trim().is_empty() {
            self.generate_group_name(&mut bucket)?
        } else {
            if bucket
                .get(&format!("group:by-name:{}", params.name))?
                .is_some()
            {
                return Err(RegistryError::Conflict(format!(
                    "group {:?} already exists",
                    params.name
                )));
            }
            params.name
        };
        let ttl_seconds = normalize_ttl(params.ttl_seconds.unwrap_or(DEFAULT_GROUP_TTL_SECONDS))?;
        let group = Group {
            id: Ulid::new().to_string(),
            name,
            implicit: false,
            created_at: Utc::now(),
            owner_id: params.owner_id,
            ttl_seconds,
            sliding_ttl: params.sliding_ttl.unwrap_or(DEFAULT_GROUP_SLIDING_TTL),
            last_activity_at: None,
        };
        write_group(&mut bucket, &group)?;
        bucket.set_ttl(&format!("group:{}", group.id), group.ttl_seconds)?;
        bucket.set_ttl(&format!("group:by-name:{}", group.name), group.ttl_seconds)?;
        Ok(group)
    }

    /// All groups, oldest-first. Admin-only callers use this; the
    /// per-owner shape is `list_groups_by_owner` below.
    pub fn list_groups(&self) -> Result<Vec<Group>, RegistryError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members("group:all")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // Walk past stragglers: if the record vanished (e.g.
            // Valkey TTL fired since we read the set), drop it
            // silently. The next sweeper pass cleans the index.
            if let Ok(g) = self.read_group(&mut bucket, &id) {
                out.push(g);
            }
        }
        Ok(out)
    }

    pub fn list_groups_by_owner(&self, owner_id: &str) -> Result<Vec<Group>, RegistryError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members(&format!("group:owner:{owner_id}"))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(g) = self.read_group(&mut bucket, &id) {
                out.push(g);
            }
        }
        Ok(out)
    }

    /// Patch a subset of mutable group fields. `ttl_seconds = Some(s)`
    /// validates against `MAX_GROUP_TTL_SECONDS` and re-arms the
    /// Valkey TTL; `sliding_ttl = Some(b)` flips the flag. Rename and
    /// owner-transfer aren't supported in this slice.
    pub fn patch_group(
        &self,
        group_id: &str,
        ttl_seconds: Option<u64>,
        sliding_ttl: Option<bool>,
    ) -> Result<Group, RegistryError> {
        let mut bucket = self.bucket()?;
        let mut group = self.read_group(&mut bucket, group_id)?;
        if let Some(ttl) = ttl_seconds {
            let ttl = normalize_ttl(ttl)?;
            group.ttl_seconds = ttl;
            bucket.hash_set(
                &format!("group:{group_id}"),
                "ttl_seconds",
                ttl.to_string().into_bytes(),
            )?;
            bucket.set_ttl(&format!("group:{group_id}"), ttl)?;
            bucket.set_ttl(&format!("group:by-name:{}", group.name), ttl)?;
        }
        if let Some(flag) = sliding_ttl {
            group.sliding_ttl = flag;
            bucket.hash_set(
                &format!("group:{group_id}"),
                "sliding_ttl",
                if flag { b"1".to_vec() } else { b"0".to_vec() },
            )?;
        }
        Ok(group)
    }

    /// Best-effort sliding-TTL bump used by the dispatch path: read
    /// the group, no-op if it doesn't exist or has `sliding_ttl =
    /// false`, otherwise re-arm the Valkey TTL. Returns `Ok(true)`
    /// when the bump fired, `Ok(false)` when it was a no-op for one
    /// of the legitimate reasons. Errors are surfaced for the caller
    /// to log and continue (the journal/dispatch path treats this as
    /// best-effort).
    pub fn refresh_group_if_sliding(&self, group_id: &str) -> Result<bool, RegistryError> {
        let mut bucket = self.bucket()?;
        let group = match self.read_group(&mut bucket, group_id) {
            Ok(g) => g,
            Err(RegistryError::NotFound) => return Ok(false),
            Err(e) => return Err(e),
        };
        if !group.sliding_ttl {
            return Ok(false);
        }
        bucket.set_ttl(&format!("group:{group_id}"), group.ttl_seconds)?;
        bucket.set_ttl(&format!("group:by-name:{}", group.name), group.ttl_seconds)?;
        Ok(true)
    }

    /// Record a matched dispatch against this route: bumps the
    /// route's `hits_total` counter, stamps `last_hit_at`, and stamps
    /// `last_activity_at` on the parent group. Called on every
    /// successful dispatch from the dispatcher. Best-effort: callers
    /// log on failure and move on (a journal write also happens
    /// alongside, so an operator notices).
    pub fn record_route_hit(
        &self,
        group_id: &str,
        route_id: &str,
        when: DateTime<Utc>,
    ) -> Result<(), RegistryError> {
        let mut bucket = self.bucket()?;
        let ts = when.to_rfc3339();
        bucket.hash_set(
            &format!("route:{route_id}"),
            "last_hit_at",
            ts.clone().into_bytes(),
        )?;
        bucket.hash_incr(&format!("route:{route_id}"), "hits_total", 1)?;
        bucket.hash_set(
            &format!("group:{group_id}"),
            "last_activity_at",
            ts.into_bytes(),
        )?;
        Ok(())
    }

    /// Reset the group's Valkey TTL to its configured `ttl_seconds`.
    /// Cheap; used by the explicit refresh endpoint.
    pub fn refresh_group(&self, group_id: &str) -> Result<Group, RegistryError> {
        let mut bucket = self.bucket()?;
        let group = self.read_group(&mut bucket, group_id)?;
        bucket.set_ttl(&format!("group:{group_id}"), group.ttl_seconds)?;
        bucket.set_ttl(&format!("group:by-name:{}", group.name), group.ttl_seconds)?;
        Ok(group)
    }

    /// Cascade-delete a group and everything it contains: routes (and
    /// their indexes + per-route kv namespace), the group's gkv
    /// namespace, the journal entries, the per-group counters, and
    /// the group record + indexes. Idempotent — multiple sweepers can
    /// call this against the same group_id without corrupting state.
    /// Also handles the "group record already gone" case (Valkey TTL
    /// fired) by cascading from `route:in-group:{group_id}` on its own.
    pub fn cascade_delete_group(&self, group_id: &str) -> Result<u64, RegistryError> {
        let mut bucket = self.bucket()?;
        // `read_group` returning NotFound is fine: the group's TTL may
        // have fired already; we still want to scrub the children.
        let group = match self.read_group(&mut bucket, group_id) {
            Ok(g) => Some(g),
            Err(RegistryError::NotFound) => None,
            Err(e) => return Err(e),
        };

        let route_ids = bucket.set_members(&format!("route:in-group:{group_id}"))?;
        let routes_deleted = route_ids.len() as u64;
        for route_id in &route_ids {
            // Best-effort read for index cleanup. A missing record
            // means another cascade beat us to it; skip the
            // index-strip block but still clean route:all etc.
            if let Ok(route) = self.read_route(&mut bucket, route_id) {
                strip_route_indexes(&mut bucket, &route)?;
            }
            // Route-private kv namespace. The prefix is the same
            // whether or not we read the route record.
            bucket.delete_with_prefix(&format!("kv:{group_id}:{route_id}:"))?;
            // Record fields.
            for field in [
                "id",
                "group_id",
                "group_name",
                "number",
                "methods",
                "path",
                "language",
                "bindings_version",
                "compiled_wasm",
                "created_at",
                "owner_id",
                "hits_total",
                "last_hit_at",
            ] {
                bucket.hash_delete(&format!("route:{route_id}"), field)?;
            }
            bucket.set_remove("route:all", route_id)?;
        }

        // Per-group containers / namespaces.
        bucket.delete(&format!("route:in-group:{group_id}"))?;
        bucket.delete_with_prefix(&format!("gkv:{group_id}:"))?;
        bucket.delete_with_prefix(&format!("journal:{group_id}:"))?;
        bucket.delete_with_prefix(&format!("journal:by-number:{group_id}:"))?;
        bucket.delete(&format!("group:counters:{group_id}"))?;

        // Group record + indexes.
        if let Some(g) = group.as_ref() {
            bucket.delete(&format!("group:by-name:{}", g.name))?;
            bucket.set_remove(&format!("group:owner:{}", g.owner_id), group_id)?;
            for field in [
                "id",
                "name",
                "implicit",
                "created_at",
                "owner_id",
                "ttl_seconds",
                "sliding_ttl",
                "last_activity_at",
            ] {
                bucket.hash_delete(&format!("group:{group_id}"), field)?;
            }
        }
        bucket.set_remove("group:all", group_id)?;

        Ok(routes_deleted)
    }

    /// List the group-shared kv entries (the `gkv:` namespace).
    /// Returns each key alongside its storage-level kind, mirroring
    /// `list_route_state` — bytes values inline, list/hash/set values
    /// summarised by length so the caller can render a compact
    /// overview. Used by the `/__ui/groups/{group}/state` page.
    pub fn list_group_state(&self, group_id: &str) -> Result<Vec<RouteStateEntry>, RegistryError> {
        let mut bucket = self.storage.group_bucket(group_id)?;
        let keys = bucket.list_keys(None)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let kind = bucket.kind(&key)?.unwrap_or("bytes");
            let entry = match kind {
                "bytes" => RouteStateEntry {
                    key: key.clone(),
                    kind: "bytes".into(),
                    value: bucket.get(&key)?,
                    length: None,
                },
                "list" => RouteStateEntry {
                    key: key.clone(),
                    kind: "list".into(),
                    value: None,
                    length: Some(bucket.list_length(&key)?),
                },
                "hash" => RouteStateEntry {
                    key: key.clone(),
                    kind: "hash".into(),
                    value: None,
                    length: Some(bucket.hash_keys(&key)?.len() as u64),
                },
                "set" => RouteStateEntry {
                    key: key.clone(),
                    kind: "set".into(),
                    value: None,
                    length: Some(bucket.set_members(&key)?.len() as u64),
                },
                other => RouteStateEntry {
                    key: key.clone(),
                    kind: other.into(),
                    value: None,
                    length: None,
                },
            };
            out.push(entry);
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Clear all per-route and per-group kv state for the group, but
    /// leave the routes themselves alive. Used by the
    /// `DELETE /__api/groups/{group}/state` endpoint.
    pub fn clear_group_state(&self, group_id: &str) -> Result<(), RegistryError> {
        let mut bucket = self.bucket()?;
        bucket.delete_with_prefix(&format!("kv:{group_id}:"))?;
        bucket.delete_with_prefix(&format!("gkv:{group_id}:"))?;
        Ok(())
    }

    /// Upsert byte values into the group's shared `gkv:` namespace
    /// (ADR-0025) — the store handlers read via `group-store`. Listed
    /// keys are written; others left untouched. Used by
    /// `PUT /__api/groups/{group}/state`.
    pub fn set_group_state(
        &self,
        group_id: &str,
        entries: std::collections::HashMap<String, Vec<u8>>,
    ) -> Result<(), RegistryError> {
        let mut bucket = self.storage.group_bucket(group_id)?;
        for (key, value) in entries {
            bucket.set(&key, value)?;
        }
        Ok(())
    }

    /// Clear all journal entries for the group; routes and state are
    /// untouched.
    pub fn clear_group_journal(&self, group_id: &str) -> Result<(), RegistryError> {
        let mut bucket = self.bucket()?;
        bucket.delete_with_prefix(&format!("journal:{group_id}:"))?;
        bucket.delete_with_prefix(&format!("journal:by-number:{group_id}:"))?;
        bucket.delete(&format!("group:counters:{group_id}"))?;
        Ok(())
    }

    /// Pick an unused, friendly, DNS-safe group name (ADR-0030). Group names
    /// double as subdomains, so this returns a valid DNS label. Tries a bare
    /// `adjective-noun` a handful of times, then walks a numeric
    /// disambiguator — which expands the space without a larger word list.
    fn generate_group_name(&self, bucket: &mut Bucket) -> Result<String, RegistryError> {
        for _ in 0..8 {
            let candidate = crate::naming::generate();
            if bucket.get(&format!("group:by-name:{candidate}"))?.is_none() {
                return Ok(candidate);
            }
        }
        let base = crate::naming::generate();
        let mut n = 2u32;
        loop {
            let candidate = format!("{base}-{n}");
            if bucket.get(&format!("group:by-name:{candidate}"))?.is_none() {
                return Ok(candidate);
            }
            n += 1;
        }
    }

    fn create_implicit_group(
        &self,
        bucket: &mut Bucket,
        owner_id: &str,
    ) -> Result<Group, RegistryError> {
        let id = Ulid::new().to_string();
        // Implicit groups get the same friendly, DNS-safe auto-name as any
        // other name-less group (ADR-0030) — the old `_route_{ulid}` scheme
        // is an illegal DNS label (leading underscore) under virtual-host
        // routing. `implicit: true` still keeps them out of default listings.
        let name = self.generate_group_name(bucket)?;
        let group = Group {
            id: id.clone(),
            name: name.clone(),
            implicit: true,
            created_at: Utc::now(),
            owner_id: owner_id.to_string(),
            ttl_seconds: DEFAULT_GROUP_TTL_SECONDS,
            sliding_ttl: DEFAULT_GROUP_SLIDING_TTL,
            last_activity_at: None,
        };
        write_group(bucket, &group)?;
        bucket.set_ttl(&format!("group:{}", group.id), group.ttl_seconds)?;
        bucket.set_ttl(&format!("group:by-name:{}", group.name), group.ttl_seconds)?;
        Ok(group)
    }

    // -- Route operations --------------------------------------------------

    #[tracing::instrument(
        name = "registry.create_route",
        skip_all,
        fields(route.path = %params.path, route.owner_id = %params.owner_id),
    )]
    pub fn create_route(&self, params: NewRoute) -> Result<Route, RegistryError> {
        // Validate inputs before touching storage.
        let new_pattern = Pattern::parse(&params.path)?;
        let new_methods = validate_methods(&params.methods)?;

        let mut bucket = self.bucket()?;

        // Resolve or create the group. Implicit groups inherit the
        // route creator as owner, the default TTL, and sliding TTL —
        // they live as long as traffic flows.
        let group = match params.group.as_deref() {
            Some(reference) => match self.resolve_group(&mut bucket, reference)? {
                Some(group_id) => self.read_group(&mut bucket, &group_id)?,
                None => return Err(RegistryError::NotFound),
            },
            None => self.create_implicit_group(&mut bucket, &params.owner_id)?,
        };

        // Pattern-shape conflict detection per route-model.md.
        self.scan_pattern_conflict(&mut bucket, &new_methods, &new_pattern)?;

        // Allocate the route's per-group sequence number.
        let n = bucket.hash_incr(
            &format!("group:counters:{}", group.id),
            "next_route_number",
            1,
        )? as u32;

        let route_id = Ulid::new().to_string();
        let route = Route {
            id: route_id.clone(),
            group_id: group.id.clone(),
            group_name: group.name.clone(),
            number: n,
            methods: params.methods.clone(),
            path: new_pattern.raw.clone(),
            language: params.language,
            bindings_version: params.bindings_version,
            compiled_wasm: params.compiled_wasm,
            source: params.source,
            created_at: Utc::now(),
            owner_id: params.owner_id,
            hits_total: 0,
            last_hit_at: None,
        };

        // Write the record + indexes.
        write_route(&mut bucket, &route)?;
        bucket.set(
            &format!("route:by-number:{}:{}", group.id, n),
            route_id.as_bytes().to_vec(),
        )?;
        bucket.set_add(&format!("route:in-group:{}", group.id), &route_id)?;
        bucket.set_add("route:all", &route_id)?;
        bucket.set_add(&format!("route:by-owner:{}", route.owner_id), &route_id)?;
        for method in &route.methods {
            bucket.set(
                &format!("route:by-method-path:{method}:{}", route.path),
                route_id.as_bytes().to_vec(),
            )?;
        }

        Ok(route)
    }

    pub fn get_route_by_slug(&self, group_ref: &str, number: u32) -> Result<Route, RegistryError> {
        let mut bucket = self.bucket()?;
        let group_id = self
            .resolve_group(&mut bucket, group_ref)?
            .ok_or(RegistryError::NotFound)?;
        let route_id_bytes = bucket
            .get(&format!("route:by-number:{group_id}:{number}"))?
            .ok_or(RegistryError::NotFound)?;
        let route_id = String::from_utf8(route_id_bytes)
            .map_err(|_| RegistryError::Malformed("route ulid".into()))?;
        self.read_route(&mut bucket, &route_id)
    }

    pub fn list_routes(&self) -> Result<Vec<Route>, RegistryError> {
        let mut bucket = self.bucket()?;
        self.list_routes_internal(&mut bucket)
    }

    /// Routes owned by `owner_id`. Reads the `route:by-owner:{owner_id}`
    /// index maintained by `create_route` / `delete_route`.
    pub fn list_routes_by_owner(&self, owner_id: &str) -> Result<Vec<Route>, RegistryError> {
        let mut bucket = self.bucket()?;
        let ids = bucket.set_members(&format!("route:by-owner:{owner_id}"))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.read_route(&mut bucket, &id)?);
        }
        Ok(out)
    }

    fn list_routes_internal(&self, bucket: &mut Bucket) -> Result<Vec<Route>, RegistryError> {
        let ids = bucket.set_members("route:all")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.read_route(bucket, &id)?);
        }
        Ok(out)
    }

    /// Walk existing routes and reject if any has overlapping methods
    /// and a segment-compatible pattern. The by-method-path exact-
    /// match index isn't sufficient — `GET /users/{id}` vs
    /// `GET /users/me` need this loop to catch them.
    fn scan_pattern_conflict(
        &self,
        bucket: &mut Bucket,
        new_methods: &Methods,
        new_pattern: &Pattern,
    ) -> Result<(), RegistryError> {
        for existing in self.list_routes_internal(bucket)? {
            let existing_pattern = Pattern::parse(&existing.path)?;
            let existing_methods = Methods(existing.methods.clone());
            if pattern::routes_conflict(
                new_methods,
                new_pattern,
                &existing_methods,
                &existing_pattern,
            ) {
                return Err(RegistryError::Conflict(format!(
                    "conflicts with {}/{} ({:?} {})",
                    existing.group_name, existing.number, existing.methods, existing.path
                )));
            }
        }
        Ok(())
    }

    /// Cheap conflict probe used by the API layer to short-circuit
    /// expensive work (e.g. sidecar TS compile) on idempotent retries.
    /// Returns the same `Conflict` error `create_route` would return.
    /// `create_route` re-runs this check authoritatively under its
    /// own bucket; this just lets callers skip burning a slow compile
    /// only to discover the slot is taken.
    pub fn precheck_create_conflict(
        &self,
        methods: &[String],
        path: &str,
    ) -> Result<(), RegistryError> {
        let new_pattern = Pattern::parse(path)?;
        let new_methods = validate_methods(methods)?;
        let mut bucket = self.bucket()?;
        self.scan_pattern_conflict(&mut bucket, &new_methods, &new_pattern)
    }

    #[tracing::instrument(
        name = "registry.delete_route",
        skip_all,
        fields(route.slug = format!("{group_ref}/{number}")),
    )]
    pub fn delete_route(&self, group_ref: &str, number: u32) -> Result<(), RegistryError> {
        let mut bucket = self.bucket()?;
        let group_id = self
            .resolve_group(&mut bucket, group_ref)?
            .ok_or(RegistryError::NotFound)?;
        let route_id_bytes = bucket
            .get(&format!("route:by-number:{group_id}:{number}"))?
            .ok_or(RegistryError::NotFound)?;
        let route_id = String::from_utf8(route_id_bytes)
            .map_err(|_| RegistryError::Malformed("route ulid".into()))?;
        let route = self.read_route(&mut bucket, &route_id)?;

        // Strip the indexes first so a partially-deleted record can't be
        // matched mid-cleanup.
        strip_route_indexes(&mut bucket, &route)?;
        // Clear the route's kv namespace so a recreated route at the
        // same path doesn't inherit stale state.
        bucket.delete_with_prefix(&format!("kv:{}:{}:", route.group_id, route.id))?;
        // Finally the record itself.
        for field in [
            "id",
            "group_id",
            "group_name",
            "number",
            "methods",
            "path",
            "language",
            "bindings_version",
            "compiled_wasm",
            "source",
            "created_at",
            "owner_id",
            "hits_total",
            "last_hit_at",
        ] {
            bucket.hash_delete(&format!("route:{route_id}"), field)?;
        }
        Ok(())
    }

    fn read_route(&self, bucket: &mut Bucket, route_id: &str) -> Result<Route, RegistryError> {
        let fields = bucket.hash_get_all(&format!("route:{route_id}"))?;
        if fields.is_empty() {
            return Err(RegistryError::NotFound);
        }
        decode_route(&fields)
    }

    /// List the per-route kv entries for the given route. Returns each
    /// key alongside its storage-level kind. For bytes-typed values
    /// the value bytes are inlined; for list/hash/set values the
    /// payload is summarised (length / field count / member count)
    /// so the caller can render a compact overview without paying for
    /// the full contents. Used by `GET /__api/routes/{group}/{n}/state`.
    pub fn list_route_state(
        &self,
        group_ref: &str,
        number: u32,
    ) -> Result<Vec<RouteStateEntry>, RegistryError> {
        let route = self.get_route_by_slug(group_ref, number)?;
        let mut bucket = self.storage.route_bucket(&route.group_id, &route.id)?;
        let keys = bucket.list_keys(None)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let kind = bucket.kind(&key)?.unwrap_or("bytes");
            let entry = match kind {
                "bytes" => RouteStateEntry {
                    key: key.clone(),
                    kind: "bytes".into(),
                    value: bucket.get(&key)?,
                    length: None,
                },
                "list" => RouteStateEntry {
                    key: key.clone(),
                    kind: "list".into(),
                    value: None,
                    length: Some(bucket.list_length(&key)?),
                },
                "hash" => RouteStateEntry {
                    key: key.clone(),
                    kind: "hash".into(),
                    value: None,
                    length: Some(bucket.hash_keys(&key)?.len() as u64),
                },
                "set" => RouteStateEntry {
                    key: key.clone(),
                    kind: "set".into(),
                    value: None,
                    length: Some(bucket.set_members(&key)?.len() as u64),
                },
                other => RouteStateEntry {
                    key: key.clone(),
                    kind: other.into(),
                    value: None,
                    length: None,
                },
            };
            out.push(entry);
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Clear the per-route kv namespace for the given route. The
    /// route itself stays; just its private state is wiped. Used by
    /// `DELETE /__api/routes/{group}/{n}/state`.
    pub fn clear_route_state(&self, group_ref: &str, number: u32) -> Result<u64, RegistryError> {
        let route = self.get_route_by_slug(group_ref, number)?;
        let mut bucket = self.bucket()?;
        let count = bucket.delete_with_prefix(&format!("kv:{}:{}:", route.group_id, route.id))?;
        Ok(count)
    }

    /// Upsert byte values into the route's private `kv:` namespace
    /// (ADR-0025). Listed keys are written at the same physical
    /// location the handler's `store` reads; other keys are left
    /// untouched. Used by `PUT /__api/routes/{group}/{n}/state`.
    pub fn set_route_state(
        &self,
        group_ref: &str,
        number: u32,
        entries: std::collections::HashMap<String, Vec<u8>>,
    ) -> Result<(), RegistryError> {
        let route = self.get_route_by_slug(group_ref, number)?;
        let mut bucket = self.storage.route_bucket(&route.group_id, &route.id)?;
        for (key, value) in entries {
            bucket.set(&key, value)?;
        }
        Ok(())
    }

    /// Replace a subset of mutable fields on an existing route. Path /
    /// methods changes re-validate pattern conflicts (excluding the
    /// route being edited from the scan) and swap the
    /// `route:by-method-path:` indexes. Compiled-wasm changes drop
    /// the component-cache entry at the API layer via
    /// `RouteTable::refresh_after_update`. `owner_id` and `number`
    /// are immutable.
    #[tracing::instrument(
        name = "registry.update_route",
        skip_all,
        fields(route.slug = format!("{group_ref}/{number}")),
    )]
    pub fn update_route(
        &self,
        group_ref: &str,
        number: u32,
        patch: PatchRoute,
    ) -> Result<Route, RegistryError> {
        let mut bucket = self.bucket()?;
        let group_id = self
            .resolve_group(&mut bucket, group_ref)?
            .ok_or(RegistryError::NotFound)?;
        let route_id_bytes = bucket
            .get(&format!("route:by-number:{group_id}:{number}"))?
            .ok_or(RegistryError::NotFound)?;
        let route_id = String::from_utf8(route_id_bytes)
            .map_err(|_| RegistryError::Malformed("route ulid".into()))?;
        let mut route = self.read_route(&mut bucket, &route_id)?;

        // Validate inputs before mutating storage. Methods/path get
        // their full validation pass; the artifact triple is trusted
        // (the API layer compiles + validates the wasm bytes already).
        let new_methods = if let Some(m) = patch.methods.as_ref() {
            validate_methods(m)?;
            m.clone()
        } else {
            route.methods.clone()
        };
        let new_path = if let Some(p) = patch.path.as_ref() {
            Pattern::parse(p)?.raw
        } else {
            route.path.clone()
        };

        let methods_changing = patch.methods.is_some();
        let path_changing = patch.path.is_some();

        // Conflict detection — only when method or path is changing.
        // Exclude the route being edited so it doesn't conflict with
        // itself. Same pattern as `create_route`.
        if methods_changing || path_changing {
            let new_pattern = Pattern::parse(&new_path)?;
            let new_methods_obj = Methods(new_methods.clone());
            for existing in self.list_routes_internal(&mut bucket)? {
                if existing.id == route.id {
                    continue;
                }
                let existing_pattern = Pattern::parse(&existing.path)?;
                let existing_methods = Methods(existing.methods.clone());
                if pattern::routes_conflict(
                    &new_methods_obj,
                    &new_pattern,
                    &existing_methods,
                    &existing_pattern,
                ) {
                    return Err(RegistryError::Conflict(format!(
                        "conflicts with {}/{} ({:?} {})",
                        existing.group_name, existing.number, existing.methods, existing.path
                    )));
                }
            }
        }

        // Strip the old by-method-path index entries before we update
        // the record so a partially-applied edit can't leave dangling
        // mappings.
        if methods_changing || path_changing {
            for method in &route.methods {
                bucket.delete(&format!("route:by-method-path:{method}:{}", route.path))?;
            }
        }

        // Apply mutations to the record. Each field handled
        // independently so the body controls exactly which fields are
        // written.
        let key = format!("route:{route_id}");
        if let Some(ref m) = patch.methods {
            route.methods = m.clone();
            bucket.hash_set(
                &key,
                "methods",
                serde_json::to_vec(&route.methods)
                    .map_err(|e| RegistryError::Malformed(format!("methods encode: {e}")))?,
            )?;
        }
        if patch.path.is_some() {
            route.path = new_path.clone();
            bucket.hash_set(&key, "path", route.path.as_bytes().to_vec())?;
        }
        if let Some(ref lang) = patch.language {
            route.language = lang.clone();
            bucket.hash_set(&key, "language", lang.as_bytes().to_vec())?;
        }
        if let Some(ref bv) = patch.bindings_version {
            route.bindings_version = bv.clone();
            bucket.hash_set(&key, "bindings_version", bv.as_bytes().to_vec())?;
        }
        if let Some(wasm) = patch.compiled_wasm {
            route.compiled_wasm = wasm.clone();
            bucket.hash_set(&key, "compiled_wasm", wasm)?;
        }
        // `Some(Some(_))` writes the new source; `Some(None)` clears the
        // field (a wasm swap on a previously source-language route).
        if let Some(src_opt) = patch.source {
            route.source = src_opt.clone();
            match src_opt {
                Some(src) => bucket.hash_set(&key, "source", src.into_bytes())?,
                None => bucket.hash_delete(&key, "source")?,
            }
        }

        // Re-add by-method-path entries for the new (method, path) set.
        if methods_changing || path_changing {
            for method in &route.methods {
                bucket.set(
                    &format!("route:by-method-path:{method}:{}", route.path),
                    route_id.as_bytes().to_vec(),
                )?;
            }
        }

        Ok(route)
    }
}

// -- Encoding / decoding ------------------------------------------------------

fn write_group(bucket: &mut Bucket, group: &Group) -> Result<(), RegistryError> {
    let key = format!("group:{}", group.id);
    bucket.hash_set(&key, "id", group.id.as_bytes().to_vec())?;
    bucket.hash_set(&key, "name", group.name.as_bytes().to_vec())?;
    bucket.hash_set(
        &key,
        "implicit",
        if group.implicit { b"1" } else { b"0" }.to_vec(),
    )?;
    bucket.hash_set(
        &key,
        "created_at",
        group.created_at.to_rfc3339().into_bytes(),
    )?;
    bucket.hash_set(&key, "owner_id", group.owner_id.as_bytes().to_vec())?;
    bucket.hash_set(
        &key,
        "ttl_seconds",
        group.ttl_seconds.to_string().into_bytes(),
    )?;
    bucket.hash_set(
        &key,
        "sliding_ttl",
        if group.sliding_ttl { b"1" } else { b"0" }.to_vec(),
    )?;
    bucket.set(
        &format!("group:by-name:{}", group.name),
        group.id.as_bytes().to_vec(),
    )?;
    bucket.set_add(&format!("group:owner:{}", group.owner_id), &group.id)?;
    bucket.set_add("group:all", &group.id)?;
    Ok(())
}

/// Strip the public-facing indexes that point at a route, leaving the
/// route record itself for the caller to clear. Used by both
/// `delete_route` and `cascade_delete_group` so the index-cleanup
/// rules live in one place.
fn strip_route_indexes(bucket: &mut Bucket, route: &Route) -> Result<(), RegistryError> {
    for method in &route.methods {
        bucket.delete(&format!("route:by-method-path:{method}:{}", route.path))?;
    }
    bucket.delete(&format!(
        "route:by-number:{}:{}",
        route.group_id, route.number
    ))?;
    bucket.set_remove(&format!("route:in-group:{}", route.group_id), &route.id)?;
    bucket.set_remove("route:all", &route.id)?;
    bucket.set_remove(&format!("route:by-owner:{}", route.owner_id), &route.id)?;
    Ok(())
}

/// Validate a configured TTL against the project-wide `MAX_GROUP_TTL_SECONDS`
/// and reject zero (which would mean "expire immediately" — almost
/// certainly a misuse).
fn normalize_ttl(ttl_seconds: u64) -> Result<u64, RegistryError> {
    if ttl_seconds == 0 {
        return Err(RegistryError::Malformed(
            "ttl_seconds must be positive".into(),
        ));
    }
    if ttl_seconds > MAX_GROUP_TTL_SECONDS {
        return Err(RegistryError::Malformed(format!(
            "ttl_seconds {ttl_seconds} exceeds max {MAX_GROUP_TTL_SECONDS}",
        )));
    }
    Ok(ttl_seconds)
}

fn write_route(bucket: &mut Bucket, route: &Route) -> Result<(), RegistryError> {
    let key = format!("route:{}", route.id);
    bucket.hash_set(&key, "id", route.id.as_bytes().to_vec())?;
    bucket.hash_set(&key, "group_id", route.group_id.as_bytes().to_vec())?;
    bucket.hash_set(&key, "group_name", route.group_name.as_bytes().to_vec())?;
    bucket.hash_set(&key, "number", route.number.to_string().into_bytes())?;
    bucket.hash_set(
        &key,
        "methods",
        serde_json::to_vec(&route.methods)
            .map_err(|e| RegistryError::Malformed(format!("methods encode: {e}")))?,
    )?;
    bucket.hash_set(&key, "path", route.path.as_bytes().to_vec())?;
    bucket.hash_set(&key, "language", route.language.as_bytes().to_vec())?;
    bucket.hash_set(
        &key,
        "bindings_version",
        route.bindings_version.as_bytes().to_vec(),
    )?;
    bucket.hash_set(&key, "compiled_wasm", route.compiled_wasm.clone())?;
    // Source is optional: only persisted for source-language routes.
    // Pre-compiled `wasm` uploads have no source to keep; we delete the
    // field so a re-read returns `None` rather than an empty string.
    match &route.source {
        Some(src) => bucket.hash_set(&key, "source", src.as_bytes().to_vec())?,
        None => bucket.hash_delete(&key, "source")?,
    }
    bucket.hash_set(
        &key,
        "created_at",
        route.created_at.to_rfc3339().into_bytes(),
    )?;
    bucket.hash_set(&key, "owner_id", route.owner_id.as_bytes().to_vec())?;
    Ok(())
}

fn decode_group(fields: &HashMap<String, Vec<u8>>) -> Result<Group, RegistryError> {
    Ok(Group {
        id: utf8(fields, "id")?,
        name: utf8(fields, "name")?,
        implicit: utf8(fields, "implicit")? == "1",
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
        owner_id: utf8(fields, "owner_id")?,
        ttl_seconds: utf8(fields, "ttl_seconds")?
            .parse()
            .map_err(|e| RegistryError::Malformed(format!("ttl_seconds: {e}")))?,
        sliding_ttl: utf8(fields, "sliding_ttl")? == "1",
        // Activity fields are optional — pre-slice-17 records won't
        // have them. Treat absent as "never".
        last_activity_at: utf8_opt(fields, "last_activity_at")
            .map(|s| parse_ts(&s))
            .transpose()?,
    })
}

fn decode_route(fields: &HashMap<String, Vec<u8>>) -> Result<Route, RegistryError> {
    Ok(Route {
        id: utf8(fields, "id")?,
        group_id: utf8(fields, "group_id")?,
        group_name: utf8(fields, "group_name")?,
        number: utf8(fields, "number")?
            .parse()
            .map_err(|e| RegistryError::Malformed(format!("number: {e}")))?,
        methods: serde_json::from_slice(
            fields
                .get("methods")
                .ok_or_else(|| RegistryError::Malformed("methods missing".into()))?,
        )
        .map_err(|e| RegistryError::Malformed(format!("methods decode: {e}")))?,
        path: utf8(fields, "path")?,
        language: utf8(fields, "language")?,
        bindings_version: utf8(fields, "bindings_version")?,
        compiled_wasm: fields
            .get("compiled_wasm")
            .cloned()
            .ok_or_else(|| RegistryError::Malformed("compiled_wasm missing".into()))?,
        // Source is optional — pre-slice-36 records won't have it, and
        // wasm-only routes never have it. Absent = `None`.
        source: utf8_opt(fields, "source"),
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
        owner_id: utf8(fields, "owner_id")?,
        // Activity fields are optional — pre-slice-17 records won't
        // have them. Treat absent hits_total as 0, last_hit_at as None.
        hits_total: utf8_opt(fields, "hits_total")
            .map(|s| {
                s.parse()
                    .map_err(|e| RegistryError::Malformed(format!("hits_total: {e}")))
            })
            .transpose()?
            .unwrap_or(0),
        last_hit_at: utf8_opt(fields, "last_hit_at")
            .map(|s| parse_ts(&s))
            .transpose()?,
    })
}

fn utf8(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<String, RegistryError> {
    let bytes = fields
        .get(name)
        .ok_or_else(|| RegistryError::Malformed(format!("field {name} missing")))?;
    String::from_utf8(bytes.clone())
        .map_err(|_| RegistryError::Malformed(format!("field {name} not utf-8")))
}

/// Optional-field variant of `utf8`: returns `None` when the field is
/// absent (not malformed). Used for fields added in later slices so
/// decoding pre-existing records doesn't fail.
fn utf8_opt(fields: &HashMap<String, Vec<u8>>, name: &str) -> Option<String> {
    fields
        .get(name)
        .and_then(|b| String::from_utf8(b.clone()).ok())
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, RegistryError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| RegistryError::Malformed(format!("created_at: {e}")))
}

fn validate_methods(methods: &[String]) -> Result<Methods, RegistryError> {
    if methods.is_empty() {
        return Err(RegistryError::InvalidMethod("(empty)".into()));
    }
    for m in methods {
        if m.is_empty()
            || (m != "ANY"
                && !m
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-'))
        {
            return Err(RegistryError::InvalidMethod(m.clone()));
        }
    }
    Ok(Methods(methods.to_vec()))
}

mod serde_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Vec::<u8>::deserialize(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_registry() -> Registry {
        Registry::new(Storage::in_memory())
    }

    fn sample_new_route(group: Option<&str>, path: &str) -> NewRoute {
        NewRoute {
            group: group.map(String::from),
            methods: vec!["POST".into()],
            path: path.into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: "test-owner".into(),
        }
    }

    #[test]
    fn implicit_group_gets_friendly_dns_safe_name() {
        let registry = fresh_registry();
        let route = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        // ADR-0030: implicit groups get an auto-assigned DNS-safe name, not
        // the old `_route_{ulid}` scheme (illegal leading underscore as a
        // subdomain label).
        assert!(
            !route.group_name.starts_with("_route_"),
            "got legacy name {:?}",
            route.group_name
        );
        assert!(
            crate::naming::is_valid_label(&route.group_name),
            "implicit group name {:?} is not a valid DNS label",
            route.group_name
        );
        assert_eq!(route.number, 1);
    }

    #[test]
    fn create_group_with_blank_name_auto_assigns_dns_safe_name() {
        let registry = fresh_registry();
        let group = registry
            .create_group(NewGroup {
                name: "".into(),
                owner_id: "o".into(),
                ttl_seconds: None,
                sliding_ttl: None,
            })
            .unwrap();
        assert!(
            crate::naming::is_valid_label(&group.name),
            "auto-assigned name {:?} is not a valid DNS label",
            group.name
        );
        // Retrievable by the assigned name.
        let by_name = registry.read_group_by_ref(&group.name).unwrap();
        assert_eq!(by_name.id, group.id);
    }

    #[test]
    fn second_route_in_implicit_group_keeps_separate_counter() {
        let registry = fresh_registry();
        let r1 = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        let r2 = registry
            .create_route(sample_new_route(None, "/v1/bar"))
            .unwrap();
        assert_ne!(r1.group_id, r2.group_id);
        assert_eq!(r1.number, 1);
        assert_eq!(r2.number, 1);
    }

    #[test]
    fn round_trip_via_slug() {
        let registry = fresh_registry();
        let created = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        let read = registry
            .get_route_by_slug(&created.group_name, created.number)
            .unwrap();
        assert_eq!(read, created);
    }

    #[test]
    fn list_returns_created_routes() {
        let registry = fresh_registry();
        let _r1 = registry
            .create_route(sample_new_route(None, "/v1/a"))
            .unwrap();
        let _r2 = registry
            .create_route(sample_new_route(None, "/v1/b"))
            .unwrap();
        let routes = registry.list_routes().unwrap();
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn delete_removes_route_and_indexes() {
        let registry = fresh_registry();
        let route = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        registry
            .delete_route(&route.group_name, route.number)
            .unwrap();
        assert!(matches!(
            registry.get_route_by_slug(&route.group_name, route.number),
            Err(RegistryError::NotFound)
        ));
        // Same (method, path) is now claimable again.
        registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
    }

    #[test]
    fn exact_method_path_conflict_rejected() {
        let registry = fresh_registry();
        registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        let err = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap_err();
        assert!(matches!(err, RegistryError::Conflict(_)));
    }

    #[test]
    fn list_routes_by_owner_returns_only_callers_routes() {
        let registry = fresh_registry();
        let alice = registry
            .create_route(NewRoute {
                group: None,
                methods: vec!["POST".into()],
                path: "/v1/alice".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"A".to_vec(),
                source: None,
                owner_id: "alice".into(),
            })
            .unwrap();
        let _bob = registry
            .create_route(NewRoute {
                group: None,
                methods: vec!["POST".into()],
                path: "/v1/bob".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"B".to_vec(),
                source: None,
                owner_id: "bob".into(),
            })
            .unwrap();
        let alice_routes = registry.list_routes_by_owner("alice").unwrap();
        assert_eq!(alice_routes.len(), 1);
        assert_eq!(alice_routes[0].id, alice.id);
        assert!(registry.list_routes_by_owner("nobody").unwrap().is_empty());
        // Deleting Alice's route empties her index.
        registry
            .delete_route(&alice.group_name, alice.number)
            .unwrap();
        assert!(registry.list_routes_by_owner("alice").unwrap().is_empty());
    }

    #[test]
    fn update_route_rewrites_methods_and_path() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        let updated = registry
            .update_route(
                &r.group_name,
                r.number,
                PatchRoute {
                    methods: Some(vec!["GET".into(), "POST".into()]),
                    path: Some("/v1/bar".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.methods, vec!["GET", "POST"]);
        assert_eq!(updated.path, "/v1/bar");
        // The route is still readable by its slug, and the old path's
        // by-method-path index entry is gone (so the same path is
        // free to claim again).
        let read = registry.get_route_by_slug(&r.group_name, r.number).unwrap();
        assert_eq!(read.path, "/v1/bar");
    }

    #[test]
    fn update_route_replaces_compiled_wasm() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        let updated = registry
            .update_route(
                &r.group_name,
                r.number,
                PatchRoute {
                    compiled_wasm: Some(b"NEW".to_vec()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.compiled_wasm, b"NEW");
        let read = registry.get_route_by_slug(&r.group_name, r.number).unwrap();
        assert_eq!(read.compiled_wasm, b"NEW");
    }

    #[test]
    fn update_route_rejects_conflict_with_another_route() {
        let registry = fresh_registry();
        let _other = registry
            .create_route(sample_new_route(None, "/v1/charges"))
            .unwrap();
        let r = registry
            .create_route(sample_new_route(None, "/v1/refunds"))
            .unwrap();
        // Moving /v1/refunds onto /v1/charges (same method) must
        // conflict; the original /v1/refunds is unchanged.
        let err = registry
            .update_route(
                &r.group_name,
                r.number,
                PatchRoute {
                    path: Some("/v1/charges".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::Conflict(_)));
        let read = registry.get_route_by_slug(&r.group_name, r.number).unwrap();
        assert_eq!(read.path, "/v1/refunds");
    }

    #[test]
    fn update_route_does_not_conflict_with_itself() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        // Same path, same methods — no-op patch but should still
        // succeed (the route doesn't conflict with itself).
        let updated = registry
            .update_route(
                &r.group_name,
                r.number,
                PatchRoute {
                    methods: Some(vec!["POST".into()]),
                    path: Some("/v1/foo".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.path, "/v1/foo");
    }

    #[test]
    fn record_route_hit_bumps_counter_and_timestamps() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        // Fresh route: zero hits, no last-hit timestamp; group has
        // no last-activity timestamp either.
        assert_eq!(r.hits_total, 0);
        assert!(r.last_hit_at.is_none());
        let g_before = registry.read_group_by_ref(&r.group_name).unwrap();
        assert!(g_before.last_activity_at.is_none());

        let t1 = Utc::now();
        registry.record_route_hit(&r.group_id, &r.id, t1).unwrap();

        let after_one = registry.get_route_by_slug(&r.group_name, r.number).unwrap();
        assert_eq!(after_one.hits_total, 1);
        assert_eq!(after_one.last_hit_at, Some(round_trip_rfc3339(t1)));
        let g_after_one = registry.read_group_by_ref(&r.group_name).unwrap();
        assert_eq!(g_after_one.last_activity_at, Some(round_trip_rfc3339(t1)));

        // Second hit: counter advances, timestamp updates.
        let t2 = t1 + chrono::Duration::seconds(1);
        registry.record_route_hit(&r.group_id, &r.id, t2).unwrap();
        let after_two = registry.get_route_by_slug(&r.group_name, r.number).unwrap();
        assert_eq!(after_two.hits_total, 2);
        assert_eq!(after_two.last_hit_at, Some(round_trip_rfc3339(t2)));
    }

    /// Round-trip through `to_rfc3339` + `parse_ts` to match what the
    /// storage layer does. Timestamps lose sub-microsecond precision
    /// crossing the wire so we compare values after the same lossy
    /// transform that decode applies.
    fn round_trip_rfc3339(ts: DateTime<Utc>) -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(&ts.to_rfc3339())
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn list_route_state_reports_bytes_and_collections() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/state"))
            .unwrap();
        // Plant a mix of value types directly into the route's bucket
        // so we exercise all four kinds in one pass.
        let mut bucket = registry.storage.route_bucket(&r.group_id, &r.id).unwrap();
        bucket.set("count", b"5".to_vec()).unwrap();
        bucket.list_push("events", b"a".to_vec()).unwrap();
        bucket.list_push("events", b"b".to_vec()).unwrap();
        bucket.hash_set("h", "f", b"v".to_vec()).unwrap();
        bucket.set_add("members", "alice").unwrap();
        bucket.set_add("members", "bob").unwrap();

        let entries = registry.list_route_state(&r.group_name, r.number).unwrap();
        // Sorted by key.
        let by_key: std::collections::HashMap<_, _> =
            entries.into_iter().map(|e| (e.key.clone(), e)).collect();
        let count = by_key.get("count").expect("count present");
        assert_eq!(count.kind, "bytes");
        assert_eq!(count.value.as_deref(), Some(b"5".as_slice()));
        let events = by_key.get("events").expect("events present");
        assert_eq!(events.kind, "list");
        assert_eq!(events.length, Some(2));
        let h = by_key.get("h").expect("hash present");
        assert_eq!(h.kind, "hash");
        assert_eq!(h.length, Some(1));
        let members = by_key.get("members").expect("set present");
        assert_eq!(members.kind, "set");
        assert_eq!(members.length, Some(2));
    }

    #[test]
    fn clear_route_state_wipes_keys() {
        let registry = fresh_registry();
        let r = registry
            .create_route(sample_new_route(None, "/v1/state"))
            .unwrap();
        let mut bucket = registry.storage.route_bucket(&r.group_id, &r.id).unwrap();
        bucket.set("a", b"1".to_vec()).unwrap();
        bucket.set("b", b"2".to_vec()).unwrap();
        drop(bucket);

        let cleared = registry.clear_route_state(&r.group_name, r.number).unwrap();
        assert_eq!(cleared, 2);
        let entries = registry.list_route_state(&r.group_name, r.number).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn update_route_missing_route_returns_not_found() {
        let registry = fresh_registry();
        let err = registry
            .update_route(
                "no-such-group",
                42,
                PatchRoute {
                    methods: Some(vec!["GET".into()]),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound));
    }

    #[test]
    fn route_numbers_within_a_group_are_not_reused() {
        let registry = fresh_registry();
        // Create a named group manually by going through the implicit path
        // and reusing its name explicitly.
        let r1 = registry
            .create_route(sample_new_route(None, "/v1/a"))
            .unwrap();
        // Now add a second route in the same group by name.
        let r2 = registry
            .create_route(NewRoute {
                group: Some(r1.group_name.clone()),
                methods: vec!["POST".into()],
                path: "/v1/b".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"B".to_vec(),
                source: None,
                owner_id: "test-owner".into(),
            })
            .unwrap();
        assert_eq!(r1.number, 1);
        assert_eq!(r2.number, 2);

        registry.delete_route(&r1.group_name, r1.number).unwrap();
        let r3 = registry
            .create_route(NewRoute {
                group: Some(r1.group_name.clone()),
                methods: vec!["POST".into()],
                path: "/v1/c".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"C".to_vec(),
                source: None,
                owner_id: "test-owner".into(),
            })
            .unwrap();
        assert_eq!(r3.number, 3, "deleted number must not be reused");
    }
}
