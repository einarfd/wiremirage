//! `WireBytes` — how a byte value crosses a JSON boundary (ADR-0025,
//! ADR-0026).
//!
//! The values are almost always text (JSON config, rule sets, request /
//! response payloads), and the primary reader/writer is an agent over
//! MCP — so the encoding is **a UTF-8 string by default**, with
//! `{ "base64": "<...>" }` as the escape hatch for genuinely-binary
//! values. Never an array-of-ints: that's both token-heavy and
//! unreadable, which matters on the agent surface.
//!
//! Used directly as a value type (handler state entries, dry-run
//! seed-state), and via the [`bytes_field`] serde adapter for
//! `Vec<u8>` fields whose *wire* form should be string-first while the
//! field stays plain bytes in Rust (request / response bodies).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A byte value as represented in JSON: a bare string (its UTF-8 bytes)
/// or `{ "base64": "<...>" }` for binary.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum WireBytes {
    /// UTF-8 text — the common case.
    Text(String),
    /// Base64-encoded bytes — for values that aren't valid UTF-8.
    Binary { base64: String },
    /// A JSON value, serialized to its UTF-8 JSON bytes
    /// (`{ "json": {"x":1} }` → the bytes `{"x":1}`). An **input-only**
    /// convenience: agents think in JSON values, not byte encodings, and an
    /// MCP client may "helpfully" parse a JSON-string argument into an object
    /// on the wire — which matched neither `Text` nor `Binary` and errored
    /// (ADR-0026 amendment). `from_bytes` never produces this variant; output
    /// stays string-first (`Text` / `Binary`).
    Json { json: serde_json::Value },
}

impl WireBytes {
    /// Decode to the raw bytes. A malformed base64 payload is the only
    /// failure.
    pub fn into_bytes(self) -> Result<Vec<u8>, base64::DecodeError> {
        match self {
            WireBytes::Text(s) => Ok(s.into_bytes()),
            WireBytes::Binary { base64 } => B64.decode(base64),
            // `serde_json::Value` always re-serializes (it can't hold NaN or
            // other non-representable values), so this is infallible.
            WireBytes::Json { json } => {
                Ok(serde_json::to_vec(&json).expect("serde_json::Value always serializes"))
            }
        }
    }

    /// Encode raw bytes to the wire form: a string when the bytes are
    /// valid UTF-8, else base64. Inverse of [`into_bytes`](Self::into_bytes)
    /// for any byte string, so round-trips are lossless.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => WireBytes::Text(s.to_owned()),
            Err(_) => WireBytes::Binary {
                base64: B64.encode(bytes),
            },
        }
    }
}

/// `#[serde(with = "crate::wire::bytes_field")]` for a `Vec<u8>` field
/// whose JSON form should be [`WireBytes`] (string-first) — request /
/// response bodies. The field stays `Vec<u8>` in Rust; only the wire
/// encoding changes.
pub mod bytes_field {
    use super::WireBytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        WireBytes::from_bytes(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        WireBytes::deserialize(d)?
            .into_bytes()
            .map_err(serde::de::Error::custom)
    }
}

/// Per-key value cap for handler state
/// (`storage-model.md::limits.handler_value_size`).
pub const MAX_STATE_VALUE_BYTES: usize = 1024 * 1024;

/// Decode a map of wire values to raw bytes, enforcing the per-key size
/// cap. On failure returns a human-readable message naming the offending
/// key; each surface wraps it in its own error type. Shared by the REST
/// and MCP state-write paths so the validation is identical.
pub fn decode_entries(
    entries: std::collections::HashMap<String, WireBytes>,
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

#[cfg(test)]
mod tests {
    use super::WireBytes;

    fn decode(v: serde_json::Value) -> Vec<u8> {
        serde_json::from_value::<WireBytes>(v)
            .expect("deserialize")
            .into_bytes()
            .expect("into_bytes")
    }

    #[test]
    fn bare_string_is_utf8_text() {
        assert_eq!(decode(serde_json::json!("hi")), b"hi");
    }

    #[test]
    fn base64_object_decodes_binary() {
        // base64("hi") == "aGk="
        assert_eq!(decode(serde_json::json!({ "base64": "aGk=" })), b"hi");
    }

    #[test]
    fn json_variant_serializes_value_to_bytes() {
        // ADR-0026 amendment: a `{json: <value>}` input becomes the value's
        // JSON bytes — the agent-friendly path around MCP-client coercion.
        assert_eq!(
            decode(serde_json::json!({ "json": {"x": 1} })),
            br#"{"x":1}"#
        );
        assert_eq!(decode(serde_json::json!({ "json": [1, 2] })), b"[1,2]");
    }

    #[test]
    fn from_bytes_stays_string_first() {
        // Output never uses the json variant.
        assert!(matches!(
            WireBytes::from_bytes(b"plain"),
            WireBytes::Text(_)
        ));
        assert!(matches!(
            WireBytes::from_bytes(&[0xff, 0xfe]),
            WireBytes::Binary { .. }
        ));
    }
}
