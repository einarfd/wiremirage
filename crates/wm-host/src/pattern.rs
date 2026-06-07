//! Path pattern parsing, matching, and conflict detection per
//! `route-model.md`.
//!
//! Path syntax: literal segments separated by `/`, with `{name}` segments
//! capturing any single non-empty segment, plus a trailing-segment matcher
//! `{name...}` (ADR-0028) valid only as the final segment — it captures the
//! remaining path (joined, possibly empty) as one param. No wildcards, no
//! regex, no optional segments. Trailing slashes are normalised away.
//!
//! Conflict semantics (ADR-0028): two **non-tail** patterns conflict iff
//! (a) they have the same number of segments, (b) their methods overlap, and
//! (c) every pair of segments at the same index is compatible — literal=literal
//! exact match, literal vs param, or param vs param. A **tail** pattern is a
//! deliberate backstop: it does NOT conflict with the specific routes beneath
//! it, only with another tail whose non-tail prefix is conflict-compatible
//! (so `/v1/{p...}` vs `/v1/{q...}` is rejected, but `/v1/{p...}` and
//! `/v2/{p...}` — or a tail and any specifics — coexist).

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
    #[error("trailing-segment matcher `{{{0}...}}` is only valid as the final path segment")]
    TailNotFinal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Param(String),
    /// Trailing-segment matcher `{name...}` (ADR-0028): only valid as the
    /// final segment; matches the prefix itself and any deeper path,
    /// capturing the joined remainder (possibly empty) under `name`.
    Tail(String),
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
                let inner = rest.strip_suffix('}').ok_or(PatternError::BadParam)?;
                if inner.is_empty() {
                    return Err(PatternError::BadParam);
                }
                // `{name...}` is the trailing-segment matcher; names can't
                // contain dots, so the `...` suffix is unambiguous.
                let (name, is_tail) = match inner.strip_suffix("...") {
                    Some(n) => (n, true),
                    None => (inner, false),
                };
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
                segments.push(if is_tail {
                    Segment::Tail(name.to_string())
                } else {
                    Segment::Param(name.to_string())
                });
            } else {
                if part.contains('{') || part.contains('}') {
                    return Err(PatternError::BadParam);
                }
                segments.push(Segment::Literal(part.to_string()));
            }
        }
        // A tail matcher is valid only as the final segment.
        if let Some(pos) = segments.iter().position(|s| matches!(s, Segment::Tail(_)))
            && pos != segments.len() - 1
        {
            let name = match &segments[pos] {
                Segment::Tail(n) => n.clone(),
                _ => unreachable!(),
            };
            return Err(PatternError::TailNotFinal(name));
        }
        Ok(Self {
            raw: trimmed.to_string(),
            segments,
        })
    }

    /// Whether this pattern ends in a trailing-segment matcher `{name...}`.
    pub fn has_tail(&self) -> bool {
        matches!(self.segments.last(), Some(Segment::Tail(_)))
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
        let parts: Vec<&str> = if trimmed == "/" {
            Vec::new()
        } else {
            trimmed[1..].split('/').collect()
        };

        // Tail patterns: the leading (non-tail) segments must match exactly,
        // then the tail captures the joined remainder (zero-or-more, so the
        // bare prefix matches too — `/v1/{p...}` matches `/v1`).
        if let Some(Segment::Tail(tail_name)) = self.segments.last() {
            let prefix = &self.segments[..self.segments.len() - 1];
            if parts.len() < prefix.len() {
                return None;
            }
            let mut captures = Vec::new();
            for (seg, part) in prefix.iter().zip(parts.iter()) {
                if part.is_empty() {
                    return None;
                }
                match seg {
                    Segment::Literal(lit) => {
                        if lit != part {
                            return None;
                        }
                    }
                    Segment::Param(name) => captures.push((name.clone(), (*part).to_string())),
                    // Parse guarantees the tail is the only/last segment.
                    Segment::Tail(_) => unreachable!("tail is only valid as the final segment"),
                }
            }
            captures.push((tail_name.clone(), parts[prefix.len()..].join("/")));
            return Some(captures);
        }

        if trimmed == "/" {
            return if self.segments.is_empty() {
                Some(Vec::new())
            } else {
                None
            };
        }
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
                Segment::Tail(_) => unreachable!("handled above"),
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

/// Whether two patterns conflict (ADR-0028 tail-aware):
///
/// - **Both non-tail:** the classic rule — same segment count and every
///   segment-pair compatible.
/// - **Tail vs non-tail:** never. A tail is a deliberate backstop for the
///   specific routes beneath it.
/// - **Both tail:** conflict iff their non-tail prefixes are conflict-
///   compatible (same count + each pair compatible) — i.e. they'd back-stop
///   the same paths ambiguously. Different prefixes (`/v1/{p...}` vs
///   `/v2/{p...}`, or differing lengths) coexist, resolved at match time by
///   longest-prefix-wins.
pub fn patterns_conflict(a: &Pattern, b: &Pattern) -> bool {
    match (a.has_tail(), b.has_tail()) {
        (false, false) => {
            a.segments.len() == b.segments.len()
                && a.segments
                    .iter()
                    .zip(b.segments.iter())
                    .all(|(sa, sb)| segments_compatible(sa, sb))
        }
        (true, true) => {
            let pa = &a.segments[..a.segments.len() - 1];
            let pb = &b.segments[..b.segments.len() - 1];
            pa.len() == pb.len()
                && pa
                    .iter()
                    .zip(pb.iter())
                    .all(|(sa, sb)| segments_compatible(sa, sb))
        }
        _ => false,
    }
}

fn segments_compatible(a: &Segment, b: &Segment) -> bool {
    match (a, b) {
        (Segment::Literal(x), Segment::Literal(y)) => x == y,
        (Segment::Literal(_), Segment::Param(_)) => true,
        (Segment::Param(_), Segment::Literal(_)) => true,
        (Segment::Param(_), Segment::Param(_)) => true,
        // Tails are compared via their prefixes in `patterns_conflict`, so a
        // tail segment never reaches the per-segment compatibility check.
        (Segment::Tail(_), _) | (_, Segment::Tail(_)) => false,
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

    // -- trailing-segment matcher (ADR-0028) --

    #[test]
    fn parse_tail_segment() {
        let p = pat("/v1/{path...}");
        assert_eq!(
            p.segments,
            vec![Segment::Literal("v1".into()), Segment::Tail("path".into())]
        );
        assert!(p.has_tail());
    }

    #[test]
    fn parse_rejects_tail_not_final() {
        assert_eq!(
            Pattern::parse("/{rest...}/tail").unwrap_err(),
            PatternError::TailNotFinal("rest".into())
        );
    }

    #[test]
    fn parse_rejects_empty_tail_name() {
        // `{...}` has an empty name.
        assert_eq!(
            Pattern::parse("/{...}").unwrap_err(),
            PatternError::BadParam
        );
    }

    #[test]
    fn tail_matches_prefix_and_deeper_paths() {
        let p = pat("/v1/{path...}");
        // Bare prefix → empty tail (zero-or-more).
        assert_eq!(p.match_path("/v1"), Some(vec![("path".into(), "".into())]));
        assert_eq!(p.match_path("/v1/"), Some(vec![("path".into(), "".into())]));
        // Deeper → joined remainder.
        assert_eq!(
            p.match_path("/v1/a/b"),
            Some(vec![("path".into(), "a/b".into())])
        );
        // Different literal prefix → no match.
        assert_eq!(p.match_path("/v2/x"), None);
    }

    #[test]
    fn root_tail_matches_everything() {
        let p = pat("/{path...}");
        assert_eq!(p.match_path("/"), Some(vec![("path".into(), "".into())]));
        assert_eq!(
            p.match_path("/a/b/c"),
            Some(vec![("path".into(), "a/b/c".into())])
        );
    }

    #[test]
    fn tail_does_not_conflict_with_specific_routes() {
        // The whole point of a backstop: it coexists with the specifics.
        assert!(!patterns_conflict(
            &pat("/v1/{path...}"),
            &pat("/v1/charges")
        ));
        assert!(!patterns_conflict(&pat("/{path...}"), &pat("/v1/charges")));
    }

    #[test]
    fn two_tails_same_prefix_conflict() {
        assert!(patterns_conflict(
            &pat("/v1/{path...}"),
            &pat("/v1/{other...}")
        ));
        assert!(patterns_conflict(&pat("/{a...}"), &pat("/{b...}")));
    }

    #[test]
    fn tails_with_different_prefixes_coexist() {
        // Different literal → resolved by longest-prefix at match time.
        assert!(!patterns_conflict(
            &pat("/v1/{path...}"),
            &pat("/v2/{path...}")
        ));
        // Different prefix length → coexist.
        assert!(!patterns_conflict(
            &pat("/v1/{path...}"),
            &pat("/{path...}")
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
