//! Egress policy for outbound handler callbacks (ADR-0034).
//!
//! Handler callouts (`host.scheduleCallback`, wired in a later slice) are
//! deployment-gated: off unless the operator sets `WM_EGRESS=on`, and even then
//! a hardcoded **default-deny of special-use / non-public IP ranges** always
//! applies as an accident guardrail (a buggy handler hitting the cloud-metadata
//! IP could leak instance creds even from a trusted user). `WM_EGRESS_ALLOW`
//! overrides that deny — the legitimate self-hosted / CI need is to *permit* an
//! internal range where the system-under-test lives; `WM_EGRESS_DENY` adds extra
//! denies for stricter operators.
//!
//! The policy is evaluated against a **resolved IP**, never a hostname string:
//! the caller resolves the target and checks every resolved address here, so a
//! hostname that resolves to a blocked IP (or DNS rebinding) can't slip past.
//! IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is normalised to its v4 form before the
//! check so a v6-mapped address can't bypass a v4 rule. (Resolution + the
//! IP-pinned connector live with the firing path in a later slice; this module
//! is the pure policy + config.)

use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use ipnet::IpNet;

/// Special-use / non-public ranges denied by default — the accident guardrail.
/// `WM_EGRESS_ALLOW` can override these (e.g. to permit an internal CI range).
/// Note `::ffff:0:0/96` (IPv4-mapped) is intentionally absent: such addresses
/// are normalised to their v4 form and matched against the v4 rules instead.
const SPECIAL_USE: &[&str] = &[
    // IPv4
    "0.0.0.0/8",      // "this network" / unspecified
    "10.0.0.0/8",     // RFC1918 private
    "100.64.0.0/10",  // CGNAT (RFC6598)
    "127.0.0.0/8",    // loopback
    "169.254.0.0/16", // link-local — includes the 169.254.169.254 metadata IP
    "172.16.0.0/12",  // RFC1918 private
    "192.0.0.0/24",   // IETF protocol assignments
    "192.168.0.0/16", // RFC1918 private
    "198.18.0.0/15",  // benchmarking
    "224.0.0.0/4",    // multicast
    "240.0.0.0/4",    // reserved (incl. 255.255.255.255 broadcast)
    // IPv6
    "::1/128",   // loopback
    "::/128",    // unspecified
    "fc00::/7",  // unique-local (ULA)
    "fe80::/10", // link-local
    "ff00::/8",  // multicast
];

/// Whether a resolved address may be the target of an outbound callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressDecision {
    Allow,
    /// Denied, with a short human-readable reason for the journal.
    Deny(&'static str),
}

/// Bad `WM_EGRESS_ALLOW` / `WM_EGRESS_DENY` config — surfaced at startup so a
/// typo fails fast rather than silently widening or narrowing egress.
#[derive(Debug, thiserror::Error)]
#[error("invalid egress CIDR list {var}: {entry:?}: {source}")]
pub struct EgressConfigError {
    pub var: &'static str,
    pub entry: String,
    #[source]
    pub source: ipnet::AddrParseError,
}

/// The resolved outbound-callback egress policy (ADR-0034).
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    enabled: bool,
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
}

impl EgressPolicy {
    /// Egress fully off — every address is denied. The default posture.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }

    /// Build from explicit parts (the testable core of [`from_env`]).
    pub fn new(enabled: bool, allow: Vec<IpNet>, deny: Vec<IpNet>) -> Self {
        Self {
            enabled,
            allow,
            deny,
        }
    }

    /// Build from the process environment: `WM_EGRESS` (`on`/`true`/`1` enables
    /// it), `WM_EGRESS_ALLOW`, `WM_EGRESS_DENY` (comma-separated CIDRs or bare
    /// IPs, v4 + v6). A malformed list is an error (fail-fast at startup).
    pub fn from_env() -> Result<Self, EgressConfigError> {
        Self::from_parts(
            std::env::var("WM_EGRESS").ok(),
            std::env::var("WM_EGRESS_ALLOW").ok(),
            std::env::var("WM_EGRESS_DENY").ok(),
        )
    }

    fn from_parts(
        enabled: Option<String>,
        allow: Option<String>,
        deny: Option<String>,
    ) -> Result<Self, EgressConfigError> {
        let enabled = matches!(
            enabled
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("on" | "true" | "1" | "yes")
        );
        Ok(Self {
            enabled,
            allow: parse_cidrs("WM_EGRESS_ALLOW", allow.as_deref())?,
            deny: parse_cidrs("WM_EGRESS_DENY", deny.as_deref())?,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Decide whether a single **resolved** address may be called. Precedence:
    /// disabled → explicit deny → explicit allow (overrides the default-deny) →
    /// special-use default-deny → allow (public).
    pub fn evaluate(&self, ip: IpAddr) -> EgressDecision {
        if !self.enabled {
            return EgressDecision::Deny(
                "egress is disabled (set WM_EGRESS=on to enable callouts)",
            );
        }
        let ip = normalize(ip);
        if self.deny.iter().any(|net| net.contains(&ip)) {
            return EgressDecision::Deny("address matches WM_EGRESS_DENY");
        }
        if self.allow.iter().any(|net| net.contains(&ip)) {
            return EgressDecision::Allow;
        }
        if special_use_ranges().iter().any(|net| net.contains(&ip)) {
            return EgressDecision::Deny("special-use / non-public address (blocked by default)");
        }
        EgressDecision::Allow
    }

    /// Check every resolved address for a target. **Deny if any** resolved IP is
    /// denied — a hostname resolving to both a public and an internal address
    /// must not be reachable. Empty input is denied (nothing resolved).
    pub fn check_resolved(&self, ips: &[IpAddr]) -> EgressDecision {
        if ips.is_empty() {
            return EgressDecision::Deny("host did not resolve to any address");
        }
        for ip in ips {
            match self.evaluate(*ip) {
                EgressDecision::Allow => {}
                deny => return deny,
            }
        }
        EgressDecision::Allow
    }
}

/// Normalise an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its v4 form so it
/// is matched against the v4 rules. Deliberately uses `to_ipv4_mapped` (only the
/// `::ffff:/96` block) and NOT `to_ipv4`, which would also fold `::1` → `0.0.0.1`
/// and thus let loopback bypass the `::1/128` deny.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match Ipv6Addr::to_ipv4_mapped(&v6) {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// The hardcoded special-use ranges, parsed. The literals are known-valid (a
/// unit test asserts every one parses), so `expect` here is unreachable.
fn special_use_ranges() -> Vec<IpNet> {
    SPECIAL_USE
        .iter()
        .map(|s| IpNet::from_str(s).expect("SPECIAL_USE literal is a valid CIDR"))
        .collect()
}

/// Parse a comma-separated list of CIDRs or bare IPs. A bare IP becomes a host
/// route (`/32` or `/128`). Empty / `None` → empty list.
fn parse_cidrs(var: &'static str, raw: Option<&str>) -> Result<Vec<IpNet>, EgressConfigError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let net = match IpNet::from_str(entry) {
            Ok(net) => net,
            // Not a CIDR — accept a bare IP as a host route.
            Err(_) => {
                let ip = IpAddr::from_str(entry).map_err(|_| EgressConfigError {
                    var,
                    entry: entry.to_string(),
                    // Re-run the CIDR parse to capture its error for the source.
                    source: IpNet::from_str(entry).unwrap_err(),
                })?;
                IpNet::from(ip)
            }
        };
        out.push(net);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    fn net(s: &str) -> IpNet {
        IpNet::from_str(s).unwrap()
    }

    fn enabled() -> EgressPolicy {
        EgressPolicy::new(true, vec![], vec![])
    }

    #[test]
    fn special_use_literals_all_parse() {
        // Guards the `expect` in special_use_ranges().
        assert_eq!(special_use_ranges().len(), SPECIAL_USE.len());
    }

    #[test]
    fn disabled_denies_everything() {
        let p = EgressPolicy::disabled();
        assert!(matches!(p.evaluate(ip("1.2.3.4")), EgressDecision::Deny(_)));
        assert!(!EgressPolicy::disabled().is_enabled());
    }

    #[test]
    fn public_is_allowed_when_enabled() {
        assert_eq!(enabled().evaluate(ip("1.1.1.1")), EgressDecision::Allow);
        assert_eq!(
            enabled().evaluate(ip("2606:4700::1111")),
            EgressDecision::Allow
        );
    }

    #[test]
    fn special_use_is_denied_by_default() {
        let p = enabled();
        for addr in [
            "127.0.0.1",       // loopback
            "169.254.169.254", // cloud metadata
            "10.1.2.3",        // private
            "172.16.0.1",      // private
            "192.168.1.1",     // private
            "100.64.0.1",      // CGNAT
            "0.0.0.0",         // unspecified
            "::1",             // v6 loopback
            "fe80::1",         // v6 link-local
            "fc00::1",         // v6 ULA
        ] {
            assert!(
                matches!(p.evaluate(ip(addr)), EgressDecision::Deny(_)),
                "{addr} should be denied by default"
            );
        }
    }

    #[test]
    fn ipv4_mapped_v6_cannot_bypass_v4_deny() {
        // ::ffff:10.0.0.1 must be treated as 10.0.0.1 (private → denied).
        assert!(matches!(
            enabled().evaluate(ip("::ffff:10.0.0.1")),
            EgressDecision::Deny(_)
        ));
        // ...and ::ffff:169.254.169.254 → metadata → denied.
        assert!(matches!(
            enabled().evaluate(ip("::ffff:169.254.169.254")),
            EgressDecision::Deny(_)
        ));
    }

    #[test]
    fn v6_loopback_is_not_folded_to_a_public_v4() {
        // Regression guard: `to_ipv4` (wrong) would fold ::1 → 0.0.0.1; we use
        // to_ipv4_mapped, so ::1 stays v6 and is caught by the ::1/128 deny.
        assert!(matches!(
            enabled().evaluate(ip("::1")),
            EgressDecision::Deny(_)
        ));
    }

    #[test]
    fn allow_override_permits_internal_range() {
        // Self-hosted/CI: permit the range the SUT lives on.
        let p = EgressPolicy::new(true, vec![net("10.0.0.0/8")], vec![]);
        assert_eq!(p.evaluate(ip("10.1.2.3")), EgressDecision::Allow);
        // A different private range is still denied.
        assert!(matches!(
            p.evaluate(ip("192.168.0.1")),
            EgressDecision::Deny(_)
        ));
        // Metadata stays denied even with a broad allow (it's not in 10/8).
        assert!(matches!(
            p.evaluate(ip("169.254.169.254")),
            EgressDecision::Deny(_)
        ));
    }

    #[test]
    fn deny_override_blocks_a_public_range_and_beats_allow() {
        let p = EgressPolicy::new(
            true,
            vec![net("203.0.113.0/24")],
            vec![net("203.0.113.0/24")],
        );
        // Deny is evaluated before allow, so deny wins on overlap.
        assert!(matches!(
            p.evaluate(ip("203.0.113.5")),
            EgressDecision::Deny(_)
        ));
    }

    #[test]
    fn check_resolved_denies_if_any_address_is_denied() {
        let p = enabled();
        // One public + one private (DNS-rebinding shape) → denied.
        assert!(matches!(
            p.check_resolved(&[ip("1.1.1.1"), ip("10.0.0.1")]),
            EgressDecision::Deny(_)
        ));
        assert_eq!(
            p.check_resolved(&[ip("1.1.1.1"), ip("8.8.8.8")]),
            EgressDecision::Allow
        );
        // Nothing resolved → denied.
        assert!(matches!(p.check_resolved(&[]), EgressDecision::Deny(_)));
    }

    #[test]
    fn from_parts_master_switch_and_cidr_parsing() {
        // Off unless WM_EGRESS is on-ish.
        assert!(
            !EgressPolicy::from_parts(None, None, None)
                .unwrap()
                .is_enabled()
        );
        assert!(
            !EgressPolicy::from_parts(Some("off".into()), None, None)
                .unwrap()
                .is_enabled()
        );
        let p = EgressPolicy::from_parts(
            Some("ON".into()),
            Some("10.0.0.0/8, 1.2.3.4".into()),
            Some("::/0".into()),
        )
        .unwrap();
        assert!(p.is_enabled());
        // Bare IP became a /32 host route → allowed past default (it's public).
        assert_eq!(p.evaluate(ip("1.2.3.4")), EgressDecision::Allow);
        // 10/8 allow-override works.
        assert_eq!(p.evaluate(ip("10.9.9.9")), EgressDecision::Allow);

        // Malformed entry fails fast.
        let err = EgressPolicy::from_parts(Some("on".into()), Some("not-an-ip".into()), None);
        assert!(err.is_err());
    }
}
