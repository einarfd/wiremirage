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
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub implicit: bool,
    pub created_at: DateTime<Utc>,
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

    fn create_implicit_group(&self, bucket: &mut Bucket) -> Result<Group, RegistryError> {
        let id = Ulid::new().to_string();
        // `_route_{suffix}` so it sorts after user-named groups and
        // visually signals "implementation detail."
        let name = format!("_route_{}", &id[id.len().saturating_sub(8)..]);
        let group = Group {
            id: id.clone(),
            name: name.clone(),
            implicit: true,
            created_at: Utc::now(),
        };
        write_group(bucket, &group)?;
        Ok(group)
    }

    // -- Route operations --------------------------------------------------

    pub fn create_route(&self, params: NewRoute) -> Result<Route, RegistryError> {
        // Validate inputs before touching storage.
        let new_pattern = Pattern::parse(&params.path)?;
        let new_methods = validate_methods(&params.methods)?;

        let mut bucket = self.bucket()?;

        // Resolve or create the group.
        let group = match params.group.as_deref() {
            Some(reference) => match self.resolve_group(&mut bucket, reference)? {
                Some(group_id) => self.read_group(&mut bucket, &group_id)?,
                None => return Err(RegistryError::NotFound),
            },
            None => self.create_implicit_group(&mut bucket)?,
        };

        // Pattern-shape conflict detection per route-model.md: walk all
        // existing routes, reject if any of them has overlapping methods
        // and a segment-compatible pattern. The by-method-path exact-match
        // index isn't sufficient — `GET /users/{id}` vs `GET /users/me`
        // need to be caught here.
        for existing in self.list_routes_internal(&mut bucket)? {
            let existing_pattern = Pattern::parse(&existing.path)?;
            let existing_methods = Methods(existing.methods.clone());
            if pattern::routes_conflict(
                &new_methods,
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
            created_at: Utc::now(),
        };

        // Write the record + indexes.
        write_route(&mut bucket, &route)?;
        bucket.set(
            &format!("route:by-number:{}:{}", group.id, n),
            route_id.as_bytes().to_vec(),
        )?;
        bucket.set_add(&format!("route:in-group:{}", group.id), &route_id)?;
        bucket.set_add("route:all", &route_id)?;
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

    fn list_routes_internal(&self, bucket: &mut Bucket) -> Result<Vec<Route>, RegistryError> {
        let ids = bucket.set_members("route:all")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.read_route(bucket, &id)?);
        }
        Ok(out)
    }

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
        for method in &route.methods {
            bucket.delete(&format!("route:by-method-path:{method}:{}", route.path))?;
        }
        bucket.delete(&format!(
            "route:by-number:{}:{}",
            route.group_id, route.number
        ))?;
        bucket.set_remove(&format!("route:in-group:{}", route.group_id), &route_id)?;
        bucket.set_remove("route:all", &route_id)?;
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
            "created_at",
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
    bucket.set(
        &format!("group:by-name:{}", group.name),
        group.id.as_bytes().to_vec(),
    )?;
    Ok(())
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
    bucket.hash_set(
        &key,
        "created_at",
        route.created_at.to_rfc3339().into_bytes(),
    )?;
    Ok(())
}

fn decode_group(fields: &HashMap<String, Vec<u8>>) -> Result<Group, RegistryError> {
    Ok(Group {
        id: utf8(fields, "id")?,
        name: utf8(fields, "name")?,
        implicit: utf8(fields, "implicit")? == "1",
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
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
        created_at: parse_ts(&utf8(fields, "created_at")?)?,
    })
}

fn utf8(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<String, RegistryError> {
    let bytes = fields
        .get(name)
        .ok_or_else(|| RegistryError::Malformed(format!("field {name} missing")))?;
    String::from_utf8(bytes.clone())
        .map_err(|_| RegistryError::Malformed(format!("field {name} not utf-8")))
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
        }
    }

    #[test]
    fn implicit_group_named_for_route() {
        let registry = fresh_registry();
        let route = registry
            .create_route(sample_new_route(None, "/v1/foo"))
            .unwrap();
        assert!(route.group_name.starts_with("_route_"));
        assert_eq!(route.number, 1);
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
            })
            .unwrap();
        assert_eq!(r3.number, 3, "deleted number must not be reused");
    }
}
