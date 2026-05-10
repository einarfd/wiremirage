//! Live-tail filters shared between `GET /__api/journal/tail` and
//! the MCP streaming tools. Keeps filter parsing + matching in one
//! place so the SSE handler and the MCP tools agree on semantics.
//!
//! Filters are conjunctive: every supplied field must match.
//! Unmatched-request events skip filters that reference per-route
//! fields (route slug / matched_pattern / status) — those don't make
//! sense without a matched route, so requiring them implicitly hides
//! the unmatched stream.

use crate::journal::{JournalEvent, JournalRecord, UnmatchedRecord};

/// Filter applied to journal events. All fields are optional; an
/// empty filter matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalFilter {
    /// Group name or ULID. Compares against `group_name` and
    /// `group_id`. Has no effect on unmatched events (they have no
    /// group).
    pub group: Option<String>,
    /// Route slug `{group}/{n}`. Matches when both the group and the
    /// route number line up. Has no effect on unmatched events.
    pub route: Option<RouteSlug>,
    /// HTTP method. Matches both handled (`request.method`) and
    /// unmatched events.
    pub method: Option<String>,
    /// Exact match against the matched pattern (handled events) —
    /// the route's path template, e.g. `/v1/charges/{id}`. Unmatched
    /// events have no matched pattern; setting this hides them.
    pub path_pattern: Option<String>,
    /// Status filter. `2xx` / `5xx` ranges or specific code.
    /// Unmatched events have no status; setting this hides them.
    pub status: Option<StatusFilter>,
}

/// `{group}/{n}` slug after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSlug {
    pub group_ref: String,
    pub number: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// 200..=299
    TwoXx,
    /// 300..=399
    ThreeXx,
    /// 400..=499
    FourXx,
    /// 500..=599
    FiveXx,
    /// Specific status code.
    Exact(u16),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FilterParseError {
    #[error("expected `{{group}}/{{n}}` route slug, got: {0:?}")]
    BadSlug(String),
    #[error("not a valid route number: {0:?}")]
    BadRouteNumber(String),
    #[error("status filter must be `2xx`/`3xx`/`4xx`/`5xx` or a specific code (got {0:?})")]
    BadStatus(String),
}

impl RouteSlug {
    pub fn parse(slug: &str) -> Result<Self, FilterParseError> {
        let (group, num) = slug
            .rsplit_once('/')
            .ok_or_else(|| FilterParseError::BadSlug(slug.to_string()))?;
        if group.is_empty() {
            return Err(FilterParseError::BadSlug(slug.to_string()));
        }
        let number = num
            .parse::<u32>()
            .map_err(|_| FilterParseError::BadRouteNumber(num.to_string()))?;
        Ok(Self {
            group_ref: group.to_string(),
            number,
        })
    }
}

impl StatusFilter {
    pub fn parse(s: &str) -> Result<Self, FilterParseError> {
        match s {
            "2xx" | "2XX" => Ok(Self::TwoXx),
            "3xx" | "3XX" => Ok(Self::ThreeXx),
            "4xx" | "4XX" => Ok(Self::FourXx),
            "5xx" | "5XX" => Ok(Self::FiveXx),
            other => other
                .parse::<u16>()
                .ok()
                .filter(|c| (100..=599).contains(c))
                .map(Self::Exact)
                .ok_or_else(|| FilterParseError::BadStatus(other.to_string())),
        }
    }

    pub fn matches(&self, status: u16) -> bool {
        match self {
            Self::TwoXx => (200..=299).contains(&status),
            Self::ThreeXx => (300..=399).contains(&status),
            Self::FourXx => (400..=499).contains(&status),
            Self::FiveXx => (500..=599).contains(&status),
            Self::Exact(code) => *code == status,
        }
    }
}

impl JournalFilter {
    /// Applies the filter to one event. Returns `true` when the
    /// event should be delivered to the consumer.
    pub fn matches(&self, event: &JournalEvent) -> bool {
        match event {
            JournalEvent::Handled(r) => self.matches_handled(r),
            JournalEvent::Unmatched(u) => self.matches_unmatched(u),
        }
    }

    fn matches_handled(&self, r: &JournalRecord) -> bool {
        if let Some(group) = &self.group
            && r.group_name != *group
            && r.group_id != *group
        {
            return false;
        }
        if let Some(route) = &self.route {
            if r.group_name != route.group_ref && r.group_id != route.group_ref {
                return false;
            }
            if r.route_number != route.number {
                return false;
            }
        }
        if let Some(method) = &self.method
            && !method.eq_ignore_ascii_case(&r.request.method)
        {
            return false;
        }
        if let Some(pat) = &self.path_pattern
            && r.matched_pattern != *pat
        {
            return false;
        }
        if let Some(status) = &self.status
            && !status.matches(r.response.status)
        {
            return false;
        }
        true
    }

    fn matches_unmatched(&self, u: &UnmatchedRecord) -> bool {
        // Per-route filters implicitly hide unmatched events.
        if self.route.is_some() || self.path_pattern.is_some() || self.status.is_some() {
            return false;
        }
        if self.group.is_some() {
            // Unmatched has no group; can't match.
            return false;
        }
        if let Some(method) = &self.method
            && !method.eq_ignore_ascii_case(&u.request.method)
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{
        HandlerLogEntry, JournalRecord, RequestEnvelope, ResourceUsage, ResponseEnvelope,
        UnmatchedRecord,
    };
    use chrono::Utc;

    fn handled(group_name: &str, n: u32, method: &str, pattern: &str, status: u16) -> JournalEvent {
        JournalEvent::Handled(Box::new(JournalRecord {
            id: "id".into(),
            number: n,
            trace_id: None,
            group_id: format!("{group_name}-id"),
            group_name: group_name.into(),
            route_id: format!("r-{n}"),
            route_number: n,
            matched_pattern: pattern.into(),
            request: RequestEnvelope {
                method: method.into(),
                path: pattern.into(),
                headers: vec![],
                body: vec![],
                original_body_size: 0,
                body_truncated: false,
            },
            response: ResponseEnvelope {
                status,
                headers: vec![],
                body: vec![],
                original_body_size: 0,
                body_truncated: false,
            },
            path_params: vec![],
            query: vec![],
            handler_logs: Vec::<HandlerLogEntry>::new(),
            duration_ms: 0,
            resources: ResourceUsage::default(),
            error: None,
            dropped_response_headers: vec![],
            created_at: Utc::now(),
        }))
    }

    fn unmatched(method: &str, path: &str) -> JournalEvent {
        JournalEvent::Unmatched(Box::new(UnmatchedRecord {
            id: "u".into(),
            number: 1,
            trace_id: None,
            created_at: Utc::now(),
            request: RequestEnvelope {
                method: method.into(),
                path: path.into(),
                headers: vec![],
                body: vec![],
                original_body_size: 0,
                body_truncated: false,
            },
            near_misses: vec![],
        }))
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = JournalFilter::default();
        assert!(f.matches(&handled("g", 1, "POST", "/v1/x", 200)));
        assert!(f.matches(&unmatched("GET", "/nope")));
    }

    #[test]
    fn group_filter_matches_by_name_and_id() {
        let f = JournalFilter {
            group: Some("stripe".into()),
            ..Default::default()
        };
        assert!(f.matches(&handled("stripe", 1, "POST", "/v1/x", 200)));
        assert!(!f.matches(&handled("twilio", 1, "POST", "/v1/x", 200)));
        // unmatched has no group → filtered out when group is set
        assert!(!f.matches(&unmatched("GET", "/nope")));
    }

    #[test]
    fn route_filter_requires_both_group_and_number() {
        let f = JournalFilter {
            route: Some(RouteSlug {
                group_ref: "stripe".into(),
                number: 7,
            }),
            ..Default::default()
        };
        assert!(f.matches(&handled("stripe", 7, "POST", "/v1/x", 200)));
        assert!(!f.matches(&handled("stripe", 8, "POST", "/v1/x", 200)));
        assert!(!f.matches(&handled("twilio", 7, "POST", "/v1/x", 200)));
    }

    #[test]
    fn method_filter_is_case_insensitive_and_applies_to_unmatched() {
        let f = JournalFilter {
            method: Some("post".into()),
            ..Default::default()
        };
        assert!(f.matches(&handled("g", 1, "POST", "/v1/x", 200)));
        assert!(!f.matches(&handled("g", 1, "GET", "/v1/x", 200)));
        assert!(f.matches(&unmatched("POST", "/anything")));
        assert!(!f.matches(&unmatched("DELETE", "/anything")));
    }

    #[test]
    fn path_pattern_is_exact_match_against_matched_pattern() {
        let f = JournalFilter {
            path_pattern: Some("/v1/charges/{id}".into()),
            ..Default::default()
        };
        assert!(f.matches(&handled("g", 1, "GET", "/v1/charges/{id}", 200)));
        // The path the request actually came in on does not satisfy
        // the filter — only the matched pattern does.
        assert!(!f.matches(&handled("g", 1, "GET", "/v1/charges", 200)));
        // path_pattern hides unmatched
        assert!(!f.matches(&unmatched("GET", "/v1/charges/{id}")));
    }

    #[test]
    fn status_filter_accepts_ranges_and_exact() {
        let two_xx = JournalFilter {
            status: Some(StatusFilter::TwoXx),
            ..Default::default()
        };
        assert!(two_xx.matches(&handled("g", 1, "POST", "/x", 200)));
        assert!(two_xx.matches(&handled("g", 1, "POST", "/x", 299)));
        assert!(!two_xx.matches(&handled("g", 1, "POST", "/x", 300)));

        let exact_503 = JournalFilter {
            status: Some(StatusFilter::Exact(503)),
            ..Default::default()
        };
        assert!(exact_503.matches(&handled("g", 1, "POST", "/x", 503)));
        assert!(!exact_503.matches(&handled("g", 1, "POST", "/x", 502)));
    }

    #[test]
    fn status_parser_handles_dsl_and_specific_codes() {
        assert_eq!(StatusFilter::parse("2xx").unwrap(), StatusFilter::TwoXx);
        assert_eq!(StatusFilter::parse("5XX").unwrap(), StatusFilter::FiveXx);
        assert_eq!(
            StatusFilter::parse("503").unwrap(),
            StatusFilter::Exact(503)
        );
        assert!(StatusFilter::parse("12345").is_err());
        assert!(StatusFilter::parse("nope").is_err());
    }

    #[test]
    fn route_slug_parser_handles_simple_and_nested_groups() {
        assert_eq!(
            RouteSlug::parse("stripe/7").unwrap(),
            RouteSlug {
                group_ref: "stripe".into(),
                number: 7,
            }
        );
        // Group name with a slash is unusual but supported (rsplit_once)
        assert_eq!(
            RouteSlug::parse("foo/bar/3").unwrap(),
            RouteSlug {
                group_ref: "foo/bar".into(),
                number: 3,
            }
        );
        assert!(RouteSlug::parse("stripe").is_err());
        assert!(RouteSlug::parse("/3").is_err());
        assert!(RouteSlug::parse("stripe/abc").is_err());
    }
}
