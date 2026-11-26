//! Parsing and matching of the `socks5-proxy-bypass` exception list, ported from
//! `ProxyBypassMatcher.cs`.

use std::collections::HashSet;
use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;

/// Destinations that connect directly even while the process-wide outbound SOCKS5 proxy
/// is configured. Entries: `*` (everything), an exact host name (`sonarr.lan`, which also
/// covers its subdomains, NO_PROXY-style), a domain suffix (`.lan` or `*.lan`, subdomains
/// only), an IP literal, or a CIDR range. IP entries match IP-literal destinations only;
/// names are never resolved here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BypassMatcher {
    match_all: bool,
    /// Lowercased exact host names.
    hosts: HashSet<String>,
    /// Lowercased domain suffixes, each with a leading dot.
    suffixes: HashSet<String>,
    addresses: HashSet<IpAddr>,
    networks: Vec<IpNet>,
}

impl BypassMatcher {
    /// True when no exception is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.match_all
            && self.hosts.is_empty()
            && self.suffixes.is_empty()
            && self.addresses.is_empty()
            && self.networks.is_empty()
    }

    /// Splits a raw list on commas and whitespace; empty entries are dropped.
    #[must_use]
    pub fn split_entries(raw: Option<&str>) -> Vec<&str> {
        let Some(raw) = raw else { return Vec::new() };
        raw.split([',', ' ', '\t', '\r', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect()
    }

    /// True when `entry` is a single valid exception.
    #[must_use]
    pub fn is_valid_entry(entry: &str) -> bool {
        parse_entry(entry).is_some()
    }

    /// Parses the raw list, skipping invalid entries with a warning. Empty or whitespace
    /// input yields an empty matcher.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        let entries = Self::split_entries(raw);
        if entries.is_empty() {
            return Self::default();
        }
        let mut matcher = Self::default();
        for entry in entries {
            match parse_entry(entry) {
                Some(Entry::All) => matcher.match_all = true,
                Some(Entry::Host(name)) => {
                    matcher.hosts.insert(name.to_lowercase());
                }
                Some(Entry::Suffix(suffix)) => {
                    matcher.suffixes.insert(suffix.to_lowercase());
                }
                Some(Entry::Address(address)) => {
                    matcher.addresses.insert(normalize(address));
                }
                Some(Entry::Network(network)) => matcher.networks.push(network),
                None => tracing::warn!(entry, "ignoring invalid proxy-bypass entry"),
            }
        }
        if matcher.is_empty() {
            Self::default()
        } else {
            matcher
        }
    }

    /// True when `host` (a DNS name or IP literal) is an excepted destination. Matching is
    /// case-insensitive; IP literals also match CIDR entries.
    #[must_use]
    pub fn matches(&self, host: &str) -> bool {
        if host.trim().is_empty() {
            return false;
        }
        if self.match_all {
            return true;
        }
        if let Ok(address) = host.parse::<IpAddr>() {
            let address = normalize(address);
            return self.addresses.contains(&address)
                || self
                    .networks
                    .iter()
                    .any(|network| network.contains(&address));
        }
        let host = host.to_lowercase();
        if self.hosts.contains(&host) {
            return true;
        }
        if self
            .suffixes
            .iter()
            .any(|suffix| host.ends_with(suffix.as_str()))
        {
            return true;
        }
        // A plain host entry also covers its subdomains (NO_PROXY semantics).
        self.hosts
            .iter()
            .any(|name| host.ends_with(&format!(".{name}")))
    }
}

enum Entry {
    All,
    Host(String),
    Suffix(String),
    Address(IpAddr),
    Network(IpNet),
}

fn parse_entry(entry: &str) -> Option<Entry> {
    if entry == "*" {
        return Some(Entry::All);
    }
    if entry.contains('/') {
        return IpNet::from_str(entry).ok().map(Entry::Network);
    }
    if let Ok(address) = IpAddr::from_str(entry) {
        return Some(Entry::Address(address));
    }

    let mut suffix_only = false;
    let mut name = entry;
    if let Some(rest) = name.strip_prefix("*.") {
        name = rest;
        suffix_only = true;
    }
    if let Some(rest) = name.strip_prefix('.') {
        name = rest;
        suffix_only = true;
    }
    // Tolerate the FQDN trailing dot.
    if let Some(stripped) = name.strip_suffix('.') {
        name = stripped;
    }
    if !is_valid_host_name(name) {
        return None;
    }
    Some(if suffix_only {
        Entry::Suffix(format!(".{name}"))
    } else {
        Entry::Host(name.to_owned())
    })
}

fn is_valid_host_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    })
}

/// Maps IPv4-mapped IPv6 addresses to their IPv4 form so both spellings match.
fn normalize(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_input_is_empty() {
        // Ported from ProxyBypassMatcherTests.Parse_EmptyInput_IsEmpty.
        for raw in [None, Some(""), Some("   ,  \t\n")] {
            assert!(BypassMatcher::parse(raw).is_empty());
        }
    }

    #[test]
    fn valid_entries_are_accepted() {
        // Ported from ProxyBypassMatcherTests.IsValidEntry_ValidEntries_Accepted.
        for entry in [
            "*",
            "sonarr.lan",
            ".internal",
            "*.corp.example",
            "192.168.1.50",
            "192.168.0.0/16",
            "fd00::/8",
            "SONARR.LAN.", // FQDN trailing dot tolerated, case-insensitive
        ] {
            assert!(
                BypassMatcher::is_valid_entry(entry),
                "expected valid: {entry}"
            );
        }
    }

    #[test]
    fn invalid_entries_are_rejected() {
        // Ported from ProxyBypassMatcherTests.IsValidEntry_InvalidEntries_Rejected.
        for entry in [
            "",
            "not a host!",    // whitespace splits into words, "host!" is invalid
            "192.168.0.0/33", // broken CIDR
            "10.0.0.0/abc",
            "http://sonarr.lan", // scheme is not a host entry
            "*foo",              // wildcard only as "*" or "*."
            "sonarr..lan",       // empty label
            ".",
            "sonarr:8989", // port is not part of a host entry
        ] {
            assert!(
                !BypassMatcher::is_valid_entry(entry),
                "expected invalid: {entry}"
            );
        }
    }

    #[test]
    fn split_entries_splits_on_commas_and_whitespace() {
        assert_eq!(
            BypassMatcher::split_entries(Some("prowlarr, hydra.lan\n192.168.1.0/24\t10.0.0.5")),
            ["prowlarr", "hydra.lan", "192.168.1.0/24", "10.0.0.5"]
        );
    }

    #[test]
    fn matches_exact_host_entry() {
        let matcher = BypassMatcher::parse(Some("sonarr.lan"));
        for (host, expected) in [
            ("sonarr.lan", true),
            ("Sonarr.LAN", true),     // case-insensitive
            ("sub.sonarr.lan", true), // plain host entry covers subdomains
            ("notsonarr.lan", false),
            ("sonarr.lan.evil.com", false),
        ] {
            assert_eq!(matcher.matches(host), expected, "{host}");
        }
    }

    #[test]
    fn matches_domain_suffix_entry() {
        for raw in [".internal", "*.internal"] {
            let matcher = BypassMatcher::parse(Some(raw));
            for (host, expected) in [
                ("nas.internal", true),
                ("deep.nas.internal", true),
                ("internal", false), // suffix entries match subdomains only
                ("notinternal", false),
                ("internal.evil.com", false),
            ] {
                assert_eq!(matcher.matches(host), expected, "{raw} vs {host}");
            }
        }
    }

    #[test]
    fn matches_ip_literal_entry() {
        let matcher = BypassMatcher::parse(Some("192.168.1.50"));
        for (host, expected) in [
            ("192.168.1.50", true),
            ("192.168.1.51", false),
            ("::ffff:192.168.1.50", true), // IPv4-mapped IPv6 normalized
        ] {
            assert_eq!(matcher.matches(host), expected, "{host}");
        }
    }

    #[test]
    fn matches_cidr_entry() {
        let matcher = BypassMatcher::parse(Some("192.168.0.0/16"));
        for (host, expected) in [
            ("192.168.1.5", true),
            ("192.168.255.254", true),
            ("192.167.1.5", false),
            ("10.0.0.1", false),
            ("fd00::1", false), // v4 CIDR does not match v6
        ] {
            assert_eq!(matcher.matches(host), expected, "{host}");
        }
    }

    #[test]
    fn matches_ipv6_cidr_entry() {
        let matcher = BypassMatcher::parse(Some("fd00::/8"));
        assert!(matcher.matches("fd12:3456::1"));
        assert!(!matcher.matches("fe80::1"));
    }

    #[test]
    fn star_matches_everything() {
        let matcher = BypassMatcher::parse(Some("*"));
        assert!(matcher.matches("anything.example.com"));
        assert!(matcher.matches("1.2.3.4"));
    }

    #[test]
    fn empty_host_never_matches() {
        let matcher = BypassMatcher::parse(Some("*"));
        assert!(!matcher.matches(""));
        assert!(!matcher.matches("   "));
    }

    #[test]
    fn invalid_entries_are_skipped_not_fatal() {
        let matcher = BypassMatcher::parse(Some("sonarr.lan, 192.168.0.0/99, broken/entry"));
        assert!(!matcher.is_empty());
        assert!(matcher.matches("sonarr.lan"));
        assert!(!matcher.matches("192.168.1.5"));
    }

    #[test]
    fn only_invalid_entries_is_empty() {
        assert!(BypassMatcher::parse(Some("broken/entry, 192.168.0.0/99")).is_empty());
    }
}
