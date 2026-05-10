//! Map our internal errors (auth, registry, journal) onto rmcp's
//! `ErrorData`. We keep the design-doc error codes (`unauthorized`,
//! `forbidden`, `not_found`, `validation_failed`, `conflict`,
//! `internal_error`) in the structured `data` field so MCP clients
//! that recognize them can branch on the same shape the REST API
//! uses, while the JSON-RPC `code` stays one of the standard
//! protocol-level numbers.

use rmcp::ErrorData;
use serde_json::json;

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

pub fn map_journal_error(err: JournalError) -> ErrorData {
    match err {
        JournalError::NotFound => not_found("not found"),
        JournalError::Storage(e) => internal(format!("storage: {e}")),
        JournalError::Malformed(msg) => internal(format!("malformed journal record: {msg}")),
    }
}
