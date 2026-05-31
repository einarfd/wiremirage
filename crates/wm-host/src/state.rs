//! `StateValue` — how a handler-state value crosses a JSON boundary
//! (ADR-0025).
//!
//! Handler state is bytes, but on the wire it's almost always text
//! (JSON config, rule sets, response templates), and the primary writer
//! is an agent over MCP — so the encoding is **a UTF-8 string by
//! default**, with `{ "base64": "<...>" }` as the escape hatch for
//! genuinely-binary values. Never an array-of-ints: that's both
//! token-heavy and unreadable, which matters on the agent surface.
//!
//! This is the value type for the writable-state API (`PUT .../state`,
//! the `?format=snapshot` read) and for dry-run seed-state
//! (`kv_overrides` / `gkv_overrides`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A byte-valued state entry as represented in JSON: a bare string
/// (stored as its UTF-8 bytes) or `{ "base64": "<...>" }` for binary.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StateValue {
    /// UTF-8 text — the common case.
    Text(String),
    /// Base64-encoded bytes — for values that aren't valid UTF-8.
    Binary { base64: String },
}

impl StateValue {
    /// Decode to the raw bytes that get stored. A malformed base64
    /// payload is the only failure.
    pub fn into_bytes(self) -> Result<Vec<u8>, base64::DecodeError> {
        match self {
            StateValue::Text(s) => Ok(s.into_bytes()),
            StateValue::Binary { base64 } => B64.decode(base64),
        }
    }

    /// Encode stored bytes back to the wire form: a string when the
    /// bytes are valid UTF-8, else base64. The inverse of
    /// [`into_bytes`](Self::into_bytes) for any byte string, so
    /// snapshot → restore round-trips.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => StateValue::Text(s.to_owned()),
            Err(_) => StateValue::Binary {
                base64: B64.encode(bytes),
            },
        }
    }
}

/// Per-key value cap for handler state
/// (`storage-model.md::limits.handler_value_size`).
pub const MAX_STATE_VALUE_BYTES: usize = 1024 * 1024;

/// Decode a map of wire values to raw bytes, enforcing the per-key size
/// cap. On failure returns a human-readable message naming the offending
/// key; each surface wraps it in its own error type. Shared by the REST
/// and MCP write paths so the validation is identical.
pub fn decode_entries(
    entries: std::collections::HashMap<String, StateValue>,
) -> Result<std::collections::HashMap<String, Vec<u8>>, String> {
    let mut out = std::collections::HashMap::with_capacity(entries.len());
    for (key, value) in entries {
        let bytes = value
            .into_bytes()
            .map_err(|e| format!("entry {key:?}: invalid base64: {e}"))?;
        if bytes.len() > MAX_STATE_VALUE_BYTES {
            return Err(format!(
                "entry {key:?}: value is {} bytes, over the {MAX_STATE_VALUE_BYTES}-byte limit",
                bytes.len()
            ));
        }
        out.insert(key, bytes);
    }
    Ok(out)
}
