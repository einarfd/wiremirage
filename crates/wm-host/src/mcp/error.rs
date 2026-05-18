//! Map our internal errors (auth, registry, journal) onto rmcp's
//! `ErrorData`. We keep the design-doc error codes (`unauthorized`,
//! `forbidden`, `not_found`, `validation_failed`, `conflict`,
//! `internal_error`) in the structured `data` field so MCP clients
//! that recognize them can branch on the same shape the REST API
//! uses, while the JSON-RPC `code` stays one of the standard
//! protocol-level numbers.

use rmcp::ErrorData;
use serde_json::json;

use crate::api_filters::FilterParseError;
use crate::journal::JournalError;
use crate::registry::RegistryError;

fn data(code: &'static str) -> Option<serde_json::Value> {
    Some(json!({ "code": code }))
}

pub fn not_found(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::resource_not_found(msg, data("not_found"))
}

pub fn forbidden(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, data("forbidden"))
}

pub fn validation(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, data("validation_failed"))
}

pub fn conflict(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, data("conflict"))
}

pub fn internal(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, data("internal_error"))
}

pub fn map_registry_error(err: RegistryError) -> ErrorData {
    match err {
        RegistryError::NotFound => not_found("not found"),
        RegistryError::Conflict(msg) => conflict(msg),
        RegistryError::InvalidPath(e) => validation(format!("invalid path pattern: {e}")),
        RegistryError::InvalidMethod(m) => validation(format!(
            "invalid method `{m}`: must be uppercase ASCII or `ANY`"
        )),
        RegistryError::Storage(e) => internal(format!("storage: {e}")),
        RegistryError::Malformed(msg) => internal(format!("malformed registry record: {msg}")),
    }
}

pub fn map_filter_error(err: FilterParseError) -> ErrorData {
    // Mirror the REST surface: `owner_id` from a non-admin caller is a
    // forbidden, everything else is validation_failed. Surface the
    // parameter name in the structured `data` payload so MCP clients
    // (and humans) can pinpoint the offending field.
    let parameter = err.parameter();
    let code = if matches!(err, FilterParseError::OwnerNonAdmin) {
        "forbidden"
    } else {
        "validation_failed"
    };
    ErrorData::invalid_params(
        err.to_string(),
        Some(json!({ "code": code, "parameter": parameter })),
    )
}

pub fn map_journal_error(err: JournalError) -> ErrorData {
    match err {
        JournalError::NotFound => not_found("not found"),
        JournalError::Storage(e) => internal(format!("storage: {e}")),
        JournalError::Malformed(msg) => internal(format!("malformed journal record: {msg}")),
    }
}

/// Translate an `ApiError` (the REST-layer error type) to MCP's
/// `ErrorData`. Used by tools that delegate to `api::create_route_core`
/// / `api::patch_route_core` so the compile-failed → diagnostics
/// propagation matches what the REST surface returns. The structured
/// `data` payload carries the same `code` strings ApiError uses
/// (`validation_failed`, `conflict`, `compile_failed`, …) plus a
/// `diagnostics` array when present.
pub fn map_api_error(err: crate::api::ApiError) -> ErrorData {
    let code = err.code();
    let message = err.message().to_string();
    let diagnostics = err.diagnostics().to_vec();
    let mut data_obj = serde_json::Map::new();
    data_obj.insert("code".into(), serde_json::Value::String(code.into()));
    if !diagnostics.is_empty() {
        data_obj.insert(
            "diagnostics".into(),
            serde_json::Value::Array(
                diagnostics
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    let data = Some(serde_json::Value::Object(data_obj));
    // Map ApiError codes to the closest rmcp error variant. The JSON-
    // RPC code on the wire follows rmcp's pick; clients should branch
    // on `data.code` instead since that's the contract WireMirage owns.
    match code {
        "not_found" => ErrorData::resource_not_found(message, data),
        "unauthorized" | "forbidden" => ErrorData::invalid_params(message, data),
        "compile_failed" | "validation_failed" | "conflict" => {
            ErrorData::invalid_params(message, data)
        }
        _ => ErrorData::internal_error(message, data),
    }
}
