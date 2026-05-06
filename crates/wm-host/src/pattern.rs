//! Path pattern parsing, matching, and conflict detection per
//! `route-model.md`.
//!
//! Path syntax: literal segments separated by `/`, with `{name}` segments
//! capturing any single non-empty segment. No wildcards, no regex, no
//! optional segments. Trailing slashes are normalised away.
//!
//! Conflict semantics: two patterns conflict iff (a) they have the same
//! number of segments, (b) their methods overlap, and (c) every pair of
//! segments at the same index is compatible — literal=literal exact match,
//! literal vs param, or param vs param.

use std::collections::HashMap;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatternError {
    #[error("path must start with /")]
    MissingLeadingSlash,
    #[error("path segment is empty (consecutive `/`?)")]
    EmptySegment,
    #[error("parameter segment must be `{{name}}` with a non-empty name")]
    BadParam,
    #[error("duplicate parameter name `{0}` within one path")]
    DuplicateParam(String),
    #[error("parameter name {0:?} contains invalid characters; expected [A-Za-z0-9_-]")]
    InvalidParamName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Param(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    /// The original input, normalised (no trailing slash except for "/").
    pub raw: String,
    pub segments: Vec<Segment>,
}

impl Pattern {
    pub fn parse(raw: &str) -> Result<Self, PatternError> {
        if !raw.starts_with('/') {
            return Err(PatternError::MissingLeadingSlash);
        }
        // Normalise: drop trailing slash unless the path is exactly "/".
        let trimmed = if raw.len() > 1 && raw.ends_with('/') {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        let mut seen = std::collections::HashSet::new();
        let mut segments = Vec::new();
        // "/" by itself parses as zero segments.
        if trimmed == "/" {
            return Ok(Self {
                raw: "/".to_string(),
                segments,
            });
        }
        for part in trimmed[1..].split('/') {
            if part.is_empty() {
                return Err(PatternError::EmptySegment);
            }
            if let Some(rest) = part.strip_prefix('{') {
                let name = rest.strip_suffix('}').ok_or(PatternError::BadParam)?;
                if name.is_empty() {
                    return Err(PatternError::BadParam);
                }
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(PatternError::InvalidParamName(name.to_string()));
                }
                if !seen.insert(name.to_string()) {
                    return Err(PatternError::DuplicateParam(name.to_string()));
                }
                segments.push(Segment::Param(name.to_string()));
            } else {
                if part.contains('{') || part.contains('}') {
                    return Err(PatternError::BadParam);
                }
                segments.push(Segment::Literal(part.to_string()));
            }
        }
        Ok(Self {
            raw: trimmed.to_string(),
            segments,
        })
    }

    /// Match a request path against this pattern. Returns `Some(captures)`
    /// when the path matches (captures empty for parameter-less patterns)
    /// or `None` when it doesn't.
    pub fn match_path(&self, path: &str) -> Option<Vec<(String, String)>> {
        if !path.starts_with('/') {
            return None;
        }
        let trimmed = if path.len() > 1 && path.ends_with('/') {
            &path[..path.len() - 1]
        } else {
            path
        };
        if trimmed == "/" {
            return if self.segments.is_empty() {
                Some(Vec::new())
            } else {
                None
            };
        }
        let parts: Vec<&str> = trimmed[1..].split('/').collect();
        if parts.len() != self.segments.len() {
            return None;
        }
        let mut captures = Vec::new();
        for (seg, part) in self.segments.iter().zip(parts.iter()) {
            if part.is_empty() {
                return None;
            }
            match seg {
                Segment::Literal(lit) => {
                    if lit != part {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    captures.push((name.clone(), (*part).to_string()));
                }
            }
        }
        Some(captures)
    }
}

/// Method spec on a route: a list of method strings, possibly the special
/// `ANY`. Methods are uppercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Methods(pub Vec<String>);

impl Methods {
    pub fn matches(&self, method: &str) -> bool {
        let m = method.to_ascii_uppercase();
        self.0
            .iter()
            .any(|x| x == "ANY" || x.eq_ignore_ascii_case(&m))
    }

    /// Two method specs overlap if either lists `ANY` or they share at
    /// least one named method.
    pub fn overlaps(&self, other: &Methods) -> bool {
        let any_a = self.0.iter().any(|m| m == "ANY");
        let any_b = other.0.iter().any(|m| m == "ANY");
        if any_a || any_b {
            return true;
        }
        self.0.iter().any(|a| other.0.iter().any(|b| a == b))
    }
}

/// Two patterns conflict iff they have the same segment count and every
/// segment-pair at the same index is "compatible" per route-model.md.
pub fn patterns_conflict(a: &Pattern, b: &Pattern) -> bool {
    if a.segments.len() != b.segments.len() {
        return false;
    }
    a.segments
        .iter()
        .zip(b.segments.iter())
        .all(|(sa, sb)| segments_compatible(sa, sb))
}

fn segments_compatible(a: &Segment, b: &Segment) -> bool {
    match (a, b) {
        (Segment::Literal(x), Segment::Literal(y)) => x == y,
        (Segment::Literal(_), Segment::Param(_)) => true,
        (Segment::Param(_), Segment::Literal(_)) => true,
        (Segment::Param(_), Segment::Param(_)) => true,
    }
}

/// Combined check: do two routes (methods + pattern) conflict?
pub fn routes_conflict(
    methods_a: &Methods,
    pattern_a: &Pattern,
    methods_b: &Methods,
    pattern_b: &Pattern,
) -> bool {
    methods_a.overlaps(methods_b) && patterns_conflict(pattern_a, pattern_b)
}

/// Extract path parameters from a request path against a pattern as a map.
/// Convenience wrapper over `Pattern::match_path` for callers that want
/// keyed access rather than positional pairs.
pub fn captures_to_map(captures: Vec<(String, String)>) -> HashMap<String, String> {
    captures.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> Pattern {
        Pattern::parse(s).expect("parse")
    }

    fn methods(ms: &[&str]) -> Methods {
        Methods(ms.iter().map(|s| s.to_string()).collect())
    }

    // -- parsing --

    #[test]
    fn parse_root() {
        let p = pat("/");
        assert!(p.segments.is_empty());
        assert_eq!(p.raw, "/");
    }

    #[test]
    fn parse_literal_only() {
        let p = pat("/v1/charges");
        assert_eq!(
            p.segments,
            vec![
                Segment::Literal("v1".into()),
                Segment::Literal("charges".into()),
            ]
        );
    }

    #[test]
    fn parse_with_params() {
        let p = pat("/users/{id}/posts/{post-id}");
        assert_eq!(
            p.segments,
            vec![
                Segment::Literal("users".into()),
                Segment::Param("id".into()),
                Segment::Literal("posts".into()),
                Segment::Param("post-id".into()),
            ]
        );
    }

    #[test]
    fn parse_strips_trailing_slash() {
        let p = pat("/v1/charges/");
        assert_eq!(p.raw, "/v1/charges");
    }

    #[test]
    fn parse_rejects_missing_leading_slash() {
        assert_eq!(
            Pattern::parse("v1/charges").unwrap_err(),
            PatternError::MissingLeadingSlash
        );
    }

    #[test]
    fn parse_rejects_empty_segment() {
        assert_eq!(
            Pattern::parse("/v1//charges").unwrap_err(),
            PatternError::EmptySegment
        );
    }

    #[test]
    fn parse_rejects_unclosed_param() {
        assert_eq!(
            Pattern::parse("/v1/{id").unwrap_err(),
            PatternError::BadParam
        );
    }

    #[test]
    fn parse_rejects_empty_param_name() {
        assert_eq!(
            Pattern::parse("/v1/{}").unwrap_err(),
            PatternError::BadParam
        );
    }

    #[test]
    fn parse_rejects_duplicate_param() {
        assert_eq!(
            Pattern::parse("/{id}/{id}").unwrap_err(),
            PatternError::DuplicateParam("id".into())
        );
    }

    #[test]
    fn parse_rejects_invalid_chars() {
        assert!(matches!(
            Pattern::parse("/{a b}").unwrap_err(),
            PatternError::InvalidParamName(_)
        ));
    }

    // -- matching --

    #[test]
    fn match_root_against_root() {
        assert_eq!(pat("/").match_path("/"), Some(vec![]));
    }

    #[test]
    fn match_literal_exact() {
        assert_eq!(pat("/v1/charges").match_path("/v1/charges"), Some(vec![]));
    }

    #[test]
    fn match_strips_trailing_slash_on_request() {
        assert_eq!(pat("/v1/charges").match_path("/v1/charges/"), Some(vec![]));
    }

    #[test]
    fn match_captures_params() {
        let p = pat("/users/{id}/posts/{post-id}");
        assert_eq!(
            p.match_path("/users/123/posts/456"),
            Some(vec![
                ("id".into(), "123".into()),
                ("post-id".into(), "456".into()),
            ])
        );
    }

    #[test]
    fn match_rejects_wrong_segment_count() {
        assert_eq!(pat("/v1/charges").match_path("/v1/charges/extra"), None);
        assert_eq!(pat("/v1/charges").match_path("/v1"), None);
    }

    #[test]
    fn match_rejects_wrong_literal() {
        assert_eq!(pat("/v1/charges").match_path("/v1/refunds"), None);
    }

    #[test]
    fn match_param_does_not_match_empty_segment() {
        // Double-slash → empty segment in the request path.
        assert_eq!(pat("/users/{id}").match_path("/users/"), None);
    }

    // -- conflict detection --

    #[test]
    fn conflict_same_literal_path() {
        assert!(patterns_conflict(&pat("/v1/charges"), &pat("/v1/charges")));
    }

    #[test]
    fn conflict_literal_vs_param_at_same_position() {
        // From the route-model.md examples: GET /users/{id} vs GET /users/me
        assert!(patterns_conflict(&pat("/users/{id}"), &pat("/users/me")));
    }

    #[test]
    fn conflict_two_params_same_shape() {
        assert!(patterns_conflict(
            &pat("/users/{id}"),
            &pat("/users/{name}")
        ));
    }

    #[test]
    fn no_conflict_different_segment_count() {
        assert!(!patterns_conflict(
            &pat("/users/{id}"),
            &pat("/users/{id}/posts")
        ));
    }

    #[test]
    fn no_conflict_different_literals() {
        assert!(!patterns_conflict(&pat("/v1/charges"), &pat("/v1/refunds")));
    }

    #[test]
    fn no_conflict_partial_literal_match() {
        // /users/foo and /accounts/bar: same shape but literal[0] differs.
        assert!(!patterns_conflict(
            &pat("/users/foo"),
            &pat("/accounts/bar")
        ));
    }

    // -- methods --

    #[test]
    fn methods_match_case_insensitive_method() {
        let m = methods(&["POST"]);
        assert!(m.matches("post"));
        assert!(m.matches("POST"));
        assert!(!m.matches("GET"));
    }

    #[test]
    fn methods_any_matches_anything() {
        let m = methods(&["ANY"]);
        assert!(m.matches("PROPFIND"));
    }

    #[test]
    fn methods_overlap_named_intersection() {
        let a = methods(&["GET", "POST"]);
        let b = methods(&["POST", "PUT"]);
        assert!(a.overlaps(&b));
        let c = methods(&["DELETE"]);
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn methods_overlap_any_with_named() {
        let any = methods(&["ANY"]);
        let named = methods(&["GET"]);
        assert!(any.overlaps(&named));
        assert!(named.overlaps(&any));
    }

    #[test]
    fn routes_conflict_combines_methods_and_pattern() {
        let p1 = pat("/v1/charges");
        let p2 = pat("/v1/charges");
        // Same path, different methods → no conflict.
        assert!(!routes_conflict(
            &methods(&["GET"]),
            &p1,
            &methods(&["POST"]),
            &p2
        ));
        // Same path, overlapping methods → conflict.
        assert!(routes_conflict(
            &methods(&["GET", "POST"]),
            &p1,
            &methods(&["POST"]),
            &p2
        ));
    }
}
