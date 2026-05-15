//! `allow` / `deny` — IPv4 / IPv6 access control.
//!
//! Rules are evaluated in declaration order. The first rule whose target
//! matches the client IP decides — `allow` passes, `deny` returns `403`.
//! If no rule matches, the request is allowed (Nginx-compatible).

use std::net::IpAddr;

#[derive(Debug, Clone, Copy)]
pub enum AccessAction {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub enum AccessTarget {
    /// Match every IP. The argument `all` in Nginx.
    All,
    /// Match a single address.
    Ip(IpAddr),
    /// Match a CIDR block: `(base address, prefix length in bits)`.
    Cidr(IpAddr, u8),
}

#[derive(Debug, Clone)]
pub struct AccessRule {
    pub action: AccessAction,
    pub target: AccessTarget,
}

impl AccessRule {
    pub fn allow(target: AccessTarget) -> Self {
        AccessRule {
            action: AccessAction::Allow,
            target,
        }
    }
    pub fn deny(target: AccessTarget) -> Self {
        AccessRule {
            action: AccessAction::Deny,
            target,
        }
    }
}

impl AccessTarget {
    pub fn matches(&self, ip: IpAddr) -> bool {
        match self {
            AccessTarget::All => true,
            AccessTarget::Ip(b) => *b == ip,
            AccessTarget::Cidr(base, bits) => cidr_match(*base, *bits, ip),
        }
    }
}

fn cidr_match(base: IpAddr, bits: u8, ip: IpAddr) -> bool {
    match (base, ip) {
        (IpAddr::V4(b), IpAddr::V4(c)) => {
            mask_match(&b.octets(), &c.octets(), bits as usize, 32)
        }
        (IpAddr::V6(b), IpAddr::V6(c)) => {
            mask_match(&b.octets(), &c.octets(), bits as usize, 128)
        }
        _ => false,
    }
}

fn mask_match(base: &[u8], ip: &[u8], bits: usize, total: usize) -> bool {
    if bits > total {
        return false;
    }
    if bits == 0 {
        return true;
    }
    let full_bytes = bits / 8;
    let tail_bits = bits % 8;
    if base[..full_bytes] != ip[..full_bytes] {
        return false;
    }
    if tail_bits == 0 {
        return true;
    }
    let mask: u8 = 0xFFu8 << (8 - tail_bits);
    (base[full_bytes] & mask) == (ip[full_bytes] & mask)
}

/// Decide whether the request is allowed. `None` rules → always allow.
pub fn check(rules: &[AccessRule], ip: IpAddr) -> bool {
    for r in rules {
        if r.target.matches(ip) {
            return matches!(r.action, AccessAction::Allow);
        }
    }
    true
}

/// Parse one `allow X;` / `deny X;` target. `X` is `all` / an IP / `IP/N`.
pub fn parse_target(s: &str) -> Result<AccessTarget, String> {
    if s == "all" {
        return Ok(AccessTarget::All);
    }
    if let Some((base, bits)) = s.split_once('/') {
        let base_ip: IpAddr = base
            .parse()
            .map_err(|_| format!("invalid CIDR base '{base}' in '{s}'"))?;
        let bits: u8 = bits
            .parse()
            .map_err(|_| format!("invalid CIDR prefix '{bits}' in '{s}'"))?;
        let max_bits = match base_ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if bits > max_bits {
            return Err(format!(
                "CIDR prefix /{bits} too large for {} (max /{max_bits})",
                base_ip
            ));
        }
        return Ok(AccessTarget::Cidr(base_ip, bits));
    }
    let ip: IpAddr = s.parse().map_err(|_| format!("invalid address '{s}'"))?;
    Ok(AccessTarget::Ip(ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn parse_all() {
        assert!(matches!(parse_target("all").unwrap(), AccessTarget::All));
    }

    #[test]
    fn parse_ip_and_match() {
        let t = parse_target("10.0.0.1").unwrap();
        assert!(t.matches(v4(10, 0, 0, 1)));
        assert!(!t.matches(v4(10, 0, 0, 2)));
    }

    #[test]
    fn parse_cidr_v4_and_match() {
        let t = parse_target("10.0.0.0/8").unwrap();
        assert!(t.matches(v4(10, 1, 2, 3)));
        assert!(!t.matches(v4(11, 1, 2, 3)));
    }

    #[test]
    fn parse_cidr_v4_partial_byte() {
        // /25 prefix splits the last octet at the high bit.
        let t = parse_target("192.168.1.0/25").unwrap();
        assert!(t.matches(v4(192, 168, 1, 0)));
        assert!(t.matches(v4(192, 168, 1, 127)));
        assert!(!t.matches(v4(192, 168, 1, 128)));
    }

    #[test]
    fn parse_cidr_v6_and_match() {
        let t = parse_target("2001:db8::/32").unwrap();
        let inside: IpAddr = IpAddr::V6("2001:db8:1::1".parse::<Ipv6Addr>().unwrap());
        let outside: IpAddr = IpAddr::V6("2001:db9::1".parse::<Ipv6Addr>().unwrap());
        assert!(t.matches(inside));
        assert!(!t.matches(outside));
    }

    #[test]
    fn first_match_wins() {
        let rules = vec![
            AccessRule::allow(parse_target("10.0.0.0/8").unwrap()),
            AccessRule::deny(parse_target("all").unwrap()),
        ];
        assert!(check(&rules, v4(10, 1, 2, 3)));
        assert!(!check(&rules, v4(192, 168, 1, 1)));
    }

    #[test]
    fn no_rules_allows() {
        assert!(check(&[], v4(1, 2, 3, 4)));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_target("nope").is_err());
        assert!(parse_target("10.0.0.0/99").is_err());
        assert!(parse_target("10.0.0.0/abc").is_err());
    }
}
