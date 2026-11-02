//! The outbound policy, evaluated before any host socket opens (DESIGN 4.8).
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    network: u32,
    mask: u32,
    ports: Option<(u16, u16)>,
    protocol: Option<Protocol>,
}

impl Rule {
    /// `10.0.0.0/8`, `192.168.1.10/32:443/tcp`, `1.2.3.4/32:8000-8100`.
    fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("bad network rule {text:?}");
        let (rest, protocol) = match text.rsplit_once('/') {
            Some((rest, "tcp")) => (rest, Some(Protocol::Tcp)),
            Some((rest, "udp")) => (rest, Some(Protocol::Udp)),
            _ => (text, None),
        };
        let (cidr, ports) = match rest.split_once(':') {
            Some((cidr, ports)) => {
                let (low, high) = ports.split_once('-').unwrap_or((ports, ports));
                let low = low.parse().map_err(|_| bad())?;
                let high = high.parse().map_err(|_| bad())?;
                if low > high {
                    return Err(bad());
                }
                (cidr, Some((low, high)))
            }
            None => (rest, None),
        };
        let (address, prefix) = cidr.split_once('/').ok_or_else(bad)?;
        let address: Ipv4Addr = address.parse().map_err(|_| bad())?;
        let prefix: u32 = prefix.parse().map_err(|_| bad())?;
        if prefix > 32 {
            return Err(bad());
        }
        let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
        Ok(Self {
            network: u32::from(address) & mask,
            mask,
            ports,
            protocol,
        })
    }

    fn matches(&self, address: Ipv4Addr, port: u16, protocol: Protocol) -> bool {
        u32::from(address) & self.mask == self.network
            && self
                .ports
                .is_none_or(|(low, high)| (low..=high).contains(&port))
            && self.protocol.is_none_or(|wanted| wanted == protocol)
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    default_allow: bool,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
}

impl Policy {
    pub fn new(default: &str, allow: &[String], deny: &[String]) -> Result<Self, String> {
        let default_allow = match default {
            "allow" => true,
            "deny" => false,
            other => {
                return Err(format!(
                    "network default must be allow or deny, not {other:?}"
                ));
            }
        };
        let parse = |rules: &[String]| -> Result<Vec<Rule>, String> {
            rules.iter().map(|r| Rule::parse(r)).collect()
        };
        Ok(Self {
            default_allow,
            allow: parse(allow)?,
            deny: parse(deny)?,
        })
    }

    /// An allow rule beats a deny rule, then the default applies. The host's own loopback and the
    /// guest network (the gateway reaches host loopback) open only by an explicit allow: a default
    /// of allow means the outside world, not the services on the machine running the VM.
    pub fn permits(&self, address: Ipv4Addr, port: u16, protocol: Protocol) -> bool {
        if self
            .allow
            .iter()
            .any(|r| r.matches(address, port, protocol))
        {
            return true;
        }
        let octets = address.octets();
        let private_to_host = octets[0] == 127 || octets[0] == 0 || octets[..3] == [10, 0, 2];
        !private_to_host
            && !self.deny.iter().any(|r| r.matches(address, port, protocol))
            && self.default_allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Protocol::{Tcp, Udp};

    fn rules(text: &[&str]) -> Vec<String> {
        text.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn allow_beats_deny_beats_default() {
        let policy = Policy::new(
            "allow",
            &rules(&["192.168.1.10/32:443/tcp"]),
            &rules(&["192.168.0.0/16"]),
        )
        .unwrap();
        let host = Ipv4Addr::new(192, 168, 1, 10);
        assert!(policy.permits(host, 443, Tcp));
        assert!(!policy.permits(host, 443, Udp));
        assert!(!policy.permits(host, 80, Tcp));
        assert!(policy.permits(Ipv4Addr::new(8, 8, 8, 8), 53, Udp));
        let closed = Policy::new("deny", &rules(&["8.8.8.0/24:50-60"]), &[]).unwrap();
        assert!(closed.permits(Ipv4Addr::new(8, 8, 8, 8), 53, Udp));
        assert!(!closed.permits(Ipv4Addr::new(8, 8, 8, 8), 80, Tcp));
        assert!(!closed.permits(Ipv4Addr::new(1, 1, 1, 1), 53, Udp));
    }

    #[test]
    fn host_loopback_needs_an_explicit_allow() {
        let open = Policy::new("allow", &[], &[]).unwrap();
        for address in [[127, 0, 0, 1], [0, 0, 0, 0], [10, 0, 2, 2]] {
            assert!(!open.permits(address.into(), 22, Tcp));
        }
        let allowed = Policy::new("allow", &rules(&["10.0.2.2/32:22/tcp"]), &[]).unwrap();
        assert!(allowed.permits([10, 0, 2, 2].into(), 22, Tcp));
        assert!(!allowed.permits([10, 0, 2, 2].into(), 23, Tcp));
    }

    #[test]
    fn bad_rules_are_refused() {
        for bad in [
            "10.0.0.0",
            "10.0.0.0/33",
            "x/8",
            "1.2.3.4/32:9-1",
            "1.2.3.4/32:70000",
        ] {
            assert!(Policy::new("allow", &rules(&[bad]), &[]).is_err(), "{bad}");
        }
        assert!(Policy::new("maybe", &[], &[]).is_err());
    }
}
