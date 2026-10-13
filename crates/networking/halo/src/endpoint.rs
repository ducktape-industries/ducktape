//! A member's address on the chain: the transport the other nodes reach it
//! over and the name they reach it at. The consensus mesh, halo and noded's
//! `init`/`join` parse it.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    Tcp,
    Tls,
    Ws,
    Wss,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

impl Scheme {
    fn parse(name: &str) -> Result<Scheme, String> {
        match name {
            "tcp" => Ok(Scheme::Tcp),
            "tls" => Ok(Scheme::Tls),
            "ws" => Ok(Scheme::Ws),
            "wss" => Ok(Scheme::Wss),
            other => Err(format!(
                "`{other}` is not a transport: an address is tcp://, tls://, ws:// or wss://"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Tcp => "tcp",
            Scheme::Tls => "tls",
            Scheme::Ws => "ws",
            Scheme::Wss => "wss",
        }
    }

    fn default_port(self) -> Option<u16> {
        match self {
            Scheme::Tcp | Scheme::Tls => None,
            Scheme::Ws => Some(80),
            Scheme::Wss => Some(443),
        }
    }
}

impl FromStr for Endpoint {
    type Err = String;

    fn from_str(text: &str) -> Result<Endpoint, String> {
        let Some((scheme, _)) = text.split_once("://") else {
            return Err(format!(
                "`{text}` names no transport: write tcp://host:port, tls://host:port, ws://host or wss://host"
            ));
        };
        let scheme = Scheme::parse(scheme)?;
        let url = url::Url::parse(text).map_err(|error| format!("`{text}`: {error}"))?;
        let host = match url.host() {
            Some(url::Host::Domain(name)) => name.to_string(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(format!("`{text}` names no host")),
        };
        if host.is_empty() {
            return Err(format!("`{text}` names no host"));
        }
        let extra = !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/");
        if extra {
            return Err(format!(
                "`{text}`: an address is a transport, a host and a port, nothing else"
            ));
        }
        let Some(port) = url.port().or(scheme.default_port()) else {
            return Err(format!("`{text}` names no port"));
        };
        Ok(Endpoint { scheme, host, port })
    }
}

impl Endpoint {
    pub fn authority(&self) -> String {
        let bracketed = self.host.contains(':');
        if bracketed {
            return format!("[{}]:{}", self.host, self.port);
        }
        format!("{}:{}", self.host, self.port)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme.name(), self.authority())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Endpoint {
        text.parse().unwrap()
    }

    #[test]
    fn every_transport_parses_with_a_name_and_a_port() {
        let cases = [
            ("tcp://node.example:9000", Scheme::Tcp, "node.example", 9000),
            ("tls://node.example:9443", Scheme::Tls, "node.example", 9443),
            ("ws://node.example:8844", Scheme::Ws, "node.example", 8844),
            ("wss://node.example", Scheme::Wss, "node.example", 443),
            ("ws://node.example", Scheme::Ws, "node.example", 80),
            ("wss://node.example/", Scheme::Wss, "node.example", 443),
            ("tcp://localhost:9000", Scheme::Tcp, "localhost", 9000),
            ("tcp://10.0.0.1:9000", Scheme::Tcp, "10.0.0.1", 9000),
            ("tcp://[::1]:9000", Scheme::Tcp, "::1", 9000),
        ];
        for (text, scheme, host, port) in cases {
            let endpoint = parsed(text);
            assert_eq!(
                endpoint,
                Endpoint {
                    scheme,
                    host: host.into(),
                    port
                },
                "{text}"
            );
            assert_eq!(parsed(&endpoint.to_string()), endpoint, "{text}");
        }
        assert_eq!(
            parsed("wss://node.example").to_string(),
            "wss://node.example:443"
        );
        assert_eq!(parsed("tcp://[::1]:9000").to_string(), "tcp://[::1]:9000");
    }

    #[test]
    fn an_address_without_a_transport_or_with_more_than_one_is_refused() {
        for text in [
            "127.0.0.1:9000",
            "node.example:9000",
            "http://node.example:8844",
            "udp://node.example:9000",
            "tcp://node.example",
            "tls://node.example",
            "tcp://:9000",
            "wss://node.example/v1/p2p",
            "wss://node.example?x=1",
            "wss://user@node.example",
        ] {
            assert!(text.parse::<Endpoint>().is_err(), "{text}");
        }
    }
}
