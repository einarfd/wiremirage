//! Shared filter, sort, and pagination primitives for the REST list
//! endpoints (`GET /api/groups`, `/api/routes`, `/api/journal/{group}`,
//! `/api/unmatched`). The vocabulary is described in
//! `rest-api.md`'s "List filtering, sorting, pagination" section;
//! this module is the implementation.
//!
//! Per-endpoint glue (which filters apply, which sort columns are
//! valid, default sort direction) lives in `api.rs`. This module
//! owns parsing + matching only.
//!
//! Filter parsing is fallible — every parser returns a typed error
//! the caller maps to `ApiError::validation` with a `details.parameter`
//! field naming the offending input.

use chrono::{DateTime, Utc};
use serde::Serialize;

// ----------------------------------------------------------------------------
// Glob matching
// ----------------------------------------------------------------------------

/// Match `target` against a `*`-glob pattern. `*` matches any
/// sequence of characters (including `/`); other characters match
/// themselves. The match is anchored — pattern `/v1/charges` matches
/// only the string `/v1/charges`, not `/v1/charges/anything`. Use
/// `*` at the end to match a prefix.
///
/// Two passes were considered: split-on-`/` segment matching vs.
/// flat substring with `*` as "anything." The spec example
/// `*` alone matches any path — i.e. `*` crosses segment
/// boundaries. Flat substring it is.
pub fn glob_match(pattern: &str, target: &str) -> bool {
    // Special-case: no wildcards → exact match.
    if !pattern.contains('*') {
        return pattern == target;
    }
    // Split on `*`, match each piece in order against the remaining
    // suffix of `target`. The first piece is anchored at the start;
    // the last piece is anchored at the end; intermediate pieces
    // just have to appear in order. This is the standard fnmatch
    // algorithm with `?` omitted (we don't support it).
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    let last_idx = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            // Adjacent stars or leading/trailing star: nothing to
            // anchor on this iteration.
            continue;
        }
        if i == 0 {
            if !target[cursor..].starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else if i == last_idx {
            return target[cursor..].ends_with(part);
        } else {
            // Find this part somewhere after cursor.
            match target[cursor..].find(part) {
                Some(pos) => cursor += pos + part.len(),
                None => return false,
            }
        }
    }
    // If the loop completed and the last part was empty (pattern
    // ended with `*`), any remaining target is fine.
    true
}

// ----------------------------------------------------------------------------
// Duration / timestamp parsing for `since` / `until`
// ----------------------------------------------------------------------------

/// Parse a `since` / `until` filter value: either an RFC 3339
/// timestamp (`2026-05-12T08:00:00Z`) or a relative duration
/// (`5m`, `1h`, `2d`, `30s`). Durations are interpreted relative
/// to `now` so the caller passes "now" in (matters for tests + for
/// the request's notion of "right now").
///
/// Suffixes accepted: `s` (seconds), `m` (minutes), `h` (hours),
/// `d` (days). Decimal values rejected — `1.5h` returns Err; use
/// `90m` instead.
pub fn parse_since(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, FilterParseError> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Ok(ts.with_timezone(&Utc));
    }
    // Try duration suffix.
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(FilterParseError::BadSince(raw.to_string()));
    }
    let (number, unit) = trimmed.split_at(trimmed.len() - 1);
    let n = number
        .parse::<i64>()
        .map_err(|_| FilterParseError::BadSince(raw.to_string()))?;
    if n < 0 {
        return Err(FilterParseError::BadSince(raw.to_string()));
    }
    let multiplier: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(FilterParseError::BadSince(raw.to_string())),
    };
    let seconds = n
        .checked_mul(multiplier)
        .ok_or_else(|| FilterParseError::BadSince(raw.to_string()))?;
    Ok(now - chrono::Duration::seconds(seconds))
}

// ----------------------------------------------------------------------------
// Errors
// ----------------------------------------------------------------------------

/// One-stop error type for filter / sort / pagination parsing.
/// `parameter()` names the offending query field so the API layer
/// can surface it in `validation_failed.details`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilterParseError {
    #[error(
        "invalid `since` / `until` value: {0:?} (use a duration like `5m` or an RFC 3339 timestamp)"
    )]
    BadSince(String),
    #[error("invalid `sort` value: {0:?}")]
    BadSort(String),
    #[error("invalid `dir` value: {0:?} (use `asc` or `desc`)")]
    BadDir(String),
    #[error("`sort` is not supported on this endpoint; ordering is fixed")]
    SortNotSupported,
    #[error("invalid `offset`: {0:?}")]
    BadOffset(String),
    #[error("invalid `limit`: {0:?}")]
    BadLimit(String),
    #[error("invalid `method`: {0:?} (uppercase ASCII, hyphens, or `ANY`)")]
    BadMethod(String),
    #[error("invalid `path_pattern`: {0:?}")]
    BadPathPattern(String),
    #[error("`owner_id` filter requires admin role")]
    OwnerNonAdmin,
}

impl FilterParseError {
    /// Which query parameter the error refers to. Used by the API
    /// layer to populate `validation_failed.details.parameter`.
    pub fn parameter(&self) -> &'static str {
        match self {
            Self::BadSince(_) => "since",
            Self::BadSort(_) | Self::SortNotSupported => "sort",
            Self::BadDir(_) => "dir",
            Self::BadOffset(_) => "offset",
            Self::BadLimit(_) => "limit",
            Self::BadMethod(_) => "method",
            Self::BadPathPattern(_) => "path_pattern",
            Self::OwnerNonAdmin => "owner_id",
        }
    }
}

// ----------------------------------------------------------------------------
// Sort + direction
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn parse(raw: Option<&str>, default: SortDir) -> Result<Self, FilterParseError> {
        match raw {
            None => Ok(default),
            Some(s) => match s.to_ascii_lowercase().as_str() {
                "asc" => Ok(Self::Asc),
                "desc" => Ok(Self::Desc),
                _ => Err(FilterParseError::BadDir(s.to_string())),
            },
        }
    }
}

// ----------------------------------------------------------------------------
// Method validation
// ----------------------------------------------------------------------------

/// Validate that `method` is uppercase ASCII (HTTP method tokens
/// per RFC 9110 + `ANY` as a wildcard). Same shape as the registry's
/// `validate_methods`, applied at filter-parse time so a bad method
/// in `?method=foo` surfaces as `validation_failed`.
pub fn validate_method(raw: &str) -> Result<String, FilterParseError> {
    if raw.is_empty() {
        return Err(FilterParseError::BadMethod(raw.to_string()));
    }
    let ok = raw == "ANY"
        || raw
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        return Err(FilterParseError::BadMethod(raw.to_string()));
    }
    Ok(raw.to_string())
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // -- glob_match --

    #[test]
    fn glob_exact_match_without_wildcard() {
        assert!(glob_match("/v1/charges", "/v1/charges"));
        assert!(!glob_match("/v1/charges", "/v1/charges/refund"));
        assert!(!glob_match("/v1/charges", "/v2/charges"));
    }

    #[test]
    fn glob_star_alone_matches_anything() {
        assert!(glob_match("*", "/v1/charges"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "/v1/charges/refund/anything"));
    }

    #[test]
    fn glob_prefix() {
        assert!(glob_match("/v1/*", "/v1/charges"));
        assert!(glob_match("/v1/*", "/v1/charges/refund"));
        assert!(!glob_match("/v1/*", "/v2/charges"));
    }

    #[test]
    fn glob_suffix() {
        assert!(glob_match("*/charges", "/v1/charges"));
        assert!(glob_match("*/charges", "/v2/charges"));
        assert!(glob_match("*/charges", "/charges"));
        assert!(!glob_match("*/charges", "/v1/refunds"));
    }

    #[test]
    fn glob_middle() {
        assert!(glob_match("/v*/charges", "/v1/charges"));
        assert!(glob_match("/v*/charges", "/v2/charges"));
        assert!(!glob_match("/v*/charges", "/x1/charges"));
    }

    #[test]
    fn glob_multiple_wildcards() {
        assert!(glob_match("*/v*/x*", "/anything/v1/xyz"));
        assert!(!glob_match("*/v*/x*", "/anything/u1/xyz"));
    }

    // -- parse_since --

    #[test]
    fn since_accepts_rfc3339() {
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 8, 0, 0).unwrap();
        let parsed = parse_since("2026-05-12T07:30:00Z", now).unwrap();
        assert_eq!(parsed, Utc.with_ymd_and_hms(2026, 5, 12, 7, 30, 0).unwrap());
    }

    #[test]
    fn since_accepts_duration_suffixes() {
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 8, 0, 0).unwrap();
        assert_eq!(
            parse_since("30s", now).unwrap(),
            now - chrono::Duration::seconds(30)
        );
        assert_eq!(
            parse_since("5m", now).unwrap(),
            now - chrono::Duration::minutes(5)
        );
        assert_eq!(
            parse_since("2h", now).unwrap(),
            now - chrono::Duration::hours(2)
        );
        assert_eq!(
            parse_since("3d", now).unwrap(),
            now - chrono::Duration::days(3)
        );
    }

    #[test]
    fn since_rejects_garbage() {
        let now = Utc::now();
        assert!(parse_since("foo", now).is_err());
        assert!(parse_since("", now).is_err());
        assert!(parse_since("5", now).is_err()); // no unit
        assert!(parse_since("5x", now).is_err()); // unknown unit
        assert!(parse_since("-5m", now).is_err()); // negative
        assert!(parse_since("1.5h", now).is_err()); // no decimals
    }

    // -- SortDir --

    #[test]
    fn sort_dir_parses_with_default() {
        assert_eq!(SortDir::parse(None, SortDir::Desc).unwrap(), SortDir::Desc);
        assert_eq!(
            SortDir::parse(Some("asc"), SortDir::Desc).unwrap(),
            SortDir::Asc
        );
        assert_eq!(
            SortDir::parse(Some("DESC"), SortDir::Asc).unwrap(),
            SortDir::Desc
        );
        assert!(SortDir::parse(Some("up"), SortDir::Asc).is_err());
    }

    // -- validate_method --

    #[test]
    fn method_accepts_uppercase_and_any() {
        assert!(validate_method("GET").is_ok());
        assert!(validate_method("ANY").is_ok());
        assert!(validate_method("X-CUSTOM").is_ok());
        assert!(validate_method("get").is_err());
        assert!(validate_method("").is_err());
        assert!(validate_method("GET POST").is_err());
    }

    // -- parameter() routing --

    #[test]
    fn parameter_routing_is_consistent() {
        assert_eq!(FilterParseError::BadSince("x".into()).parameter(), "since");
        assert_eq!(FilterParseError::BadSort("x".into()).parameter(), "sort");
        assert_eq!(FilterParseError::SortNotSupported.parameter(), "sort");
        assert_eq!(FilterParseError::OwnerNonAdmin.parameter(), "owner_id");
    }
}
