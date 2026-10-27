//! Invite links: `kuulu://<host>:<port>/join?t=<token>&fp=<sha256>`.
//!
//! The same link with `https://` is accepted too, so a link shared on the
//! web can be pasted as is; host and port then still name the Kuulu server.

use std::net::SocketAddr;
use url::Url;

/// UDP port a link without one points at, as in docs/PLAN.md.
pub const DEFAULT_PORT: u16 = 7700;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// Domain name or IP address, without brackets.
    pub host: String,
    pub port: u16,
    pub token: String,
    /// SHA-256 of the server certificate, lowercase hex. The connection
    /// trusts exactly this certificate.
    pub fingerprint: String,
}

impl Invite {
    /// Parses a link, with an error worded for the user.
    pub fn parse(link: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("This is not a valid Kuulu invite link: {why}.");
        let url = Url::parse(link.trim()).map_err(|_| bad("it is not a link"))?;
        if !matches!(url.scheme(), "kuulu" | "https") {
            return Err(bad("it must start with kuulu:// or https://"));
        }
        let host = match url.host() {
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            Some(host) => host.to_string(),
            None => return Err(bad("it has no server address")),
        };
        if url.path() != "/join" {
            return Err(bad("it does not point at /join"));
        }
        let query = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        let token = query("t")
            .filter(|t| !t.is_empty())
            .ok_or_else(|| bad("the invite code is missing"))?;
        let fingerprint = query("fp")
            .filter(|fp| fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| bad("the server fingerprint is missing or malformed"))?
            .to_ascii_lowercase();
        Ok(Self {
            host,
            // `Url::port` omits the scheme's default (443 for https), so an
            // https link without a port means the Kuulu default too.
            port: url.port().unwrap_or(DEFAULT_PORT),
            token,
            fingerprint,
        })
    }

    /// `host:port` as shown to the user.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The `kuulu://` link for this invite.
    pub fn link(&self) -> String {
        let mut url = Url::parse(&format!("kuulu://{}/join", self.address())).expect("valid host");
        url.query_pairs_mut()
            .append_pair("t", &self.token)
            .append_pair("fp", &self.fingerprint);
        url.into()
    }

    /// Looks up the server address.
    pub async fn resolve(&self) -> Result<SocketAddr, String> {
        tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .ok()
            .and_then(|mut addrs| addrs.next())
            .ok_or_else(|| format!("Cannot find the server {}.", self.host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "AB01cd23ef45ab01cd23ef45ab01cd23ef45ab01cd23ef45ab01cd23ef45ab01";

    #[test]
    fn parses_kuulu_and_https_links() {
        let invite = Invite::parse(&format!(
            " kuulu://perjantai.example:7701/join?t=abc&fp={FP}\n"
        ))
        .unwrap();
        assert_eq!(
            invite,
            Invite {
                host: "perjantai.example".into(),
                port: 7701,
                token: "abc".into(),
                fingerprint: FP.to_ascii_lowercase(),
            }
        );
        assert_eq!(invite.address(), "perjantai.example:7701");

        let https =
            Invite::parse(&format!("https://perjantai.example/join?fp={FP}&t=abc")).unwrap();
        assert_eq!(https.port, DEFAULT_PORT);
        assert_eq!(https.token, "abc");

        let v6 = Invite::parse(&format!("kuulu://[::1]:7700/join?t=x&fp={FP}")).unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.address(), "[::1]:7700");

        for invite in [invite, https, v6] {
            assert_eq!(Invite::parse(&invite.link()).unwrap(), invite);
        }
        assert_eq!(
            Invite::parse(&format!("kuulu://h:1/join?t=x&fp={FP}"))
                .unwrap()
                .link(),
            format!("kuulu://h:1/join?t=x&fp={}", FP.to_ascii_lowercase())
        );
    }

    #[test]
    fn rejects_malformed_links() {
        for link in [
            "",
            "perjantai.example",
            &format!("http://h:1/join?t=x&fp={FP}"),
            &format!("kuulu://h:1/other?t=x&fp={FP}"),
            &format!("kuulu://h:1/join?fp={FP}"),
            &format!("kuulu://h:1/join?t=&fp={FP}"),
            "kuulu://h:1/join?t=x",
            "kuulu://h:1/join?t=x&fp=1234",
            &format!("kuulu://h:1/join?t=x&fp={}", "z".repeat(64)),
        ] {
            let err = Invite::parse(link).unwrap_err();
            assert!(
                err.starts_with("This is not a valid Kuulu invite link"),
                "{link}: {err}"
            );
        }
    }
}
