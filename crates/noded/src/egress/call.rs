//! One request answered: a relay at once with nothing; a public `http` request at its own URL,
//! public addresses only unless the node allows private ones; a registered service's request at
//! the service's base joined with its path, with the service's secrets and the chain's word on
//! who asked. The whole response is the answer, a redirect included, never followed.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use abi::role::egress::http::{Head, Response};
use abi::role::egress::{Answer, HTTP, Request};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use url::Url;

use crate::egress::config::{Config, Reach, Upstream};

pub struct Clients {
    internet: reqwest::Client,
    services: reqwest::Client,
}

impl Clients {
    pub fn new(reach: Reach) -> Result<Clients, String> {
        let internet = match reach {
            Reach::Public => builder().dns_resolver(Arc::new(PublicOnly)),
            Reach::Private => builder(),
        };
        let built = |builder: reqwest::ClientBuilder| {
            builder
                .build()
                .map_err(|error| format!("the HTTP client does not build: {error}"))
        };
        Ok(Clients {
            internet: built(internet)?,
            services: built(builder())?,
        })
    }
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .no_proxy()
}

struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|address| public(address.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            let addresses: Addrs = Box::new(public.into_iter());
            Ok(addresses)
        })
    }
}

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => public_v4(mapped),
            None => public_v6(ip),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let this_network = a == 0;
    let shared = a == 100 && (64..128).contains(&b);
    let protocol = a == 192 && b == 0 && c == 0;
    let benchmarking = a == 198 && (18..20).contains(&b);
    let reserved = a >= 240;
    let special = this_network || shared || protocol || benchmarking || reserved;
    let local = ip.is_loopback() || ip.is_private() || ip.is_link_local();
    let unrouted = ip.is_multicast() || ip.is_documentation();
    !(special || local || unrouted)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let local = ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local();
    let unrouted = ip.is_unspecified() || ip.is_multicast();
    !(local || unrouted)
}

pub struct Caller<'a> {
    pub chain: &'a str,
    pub program: &'a str,
    pub id: u64,
}

impl Caller<'_> {
    fn headers(&self) -> [(String, String); 3] {
        [
            ("ducktape-chain".to_owned(), self.chain.to_owned()),
            ("ducktape-program".to_owned(), self.program.to_owned()),
            ("ducktape-request".to_owned(), self.id.to_string()),
        ]
    }
}

pub async fn answer(
    clients: &Clients,
    config: &Config,
    caller: &Caller<'_>,
    request: &Request,
    body: Vec<u8>,
) -> Answer {
    let sent = match request {
        Request::Relay { .. } => return relay(config),
        Request::Http { service, head, .. } => match service.as_str() {
            HTTP => to_internet(clients, config, head, body).await,
            named => to_service(clients, config, caller, named, head, body).await,
        },
    };
    match sent {
        Ok(response) => Answer::Output(abi::encode(&response)),
        Err(reason) => Answer::Failed(reason),
    }
}

fn relay(config: &Config) -> Answer {
    match config.relay {
        true => Answer::Output(Vec::new()),
        false => Answer::Failed("this node does not serve relay".to_owned()),
    }
}

fn literal(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

async fn to_internet(
    clients: &Clients,
    config: &Config,
    head: &Head,
    body: Vec<u8>,
) -> Result<Response, String> {
    if !config.http {
        return Err(format!("this node does not serve {HTTP}"));
    }
    head.check_url().map_err(|refusal| refusal.sentence)?;
    let url =
        Url::parse(&head.target).map_err(|error| format!("the URL does not parse: {error}"))?;
    let private_literal = literal(&url).is_some_and(|ip| !public(ip));
    if config.reach == Reach::Public && private_literal {
        return Err(format!("{url} is not a public address"));
    }
    send(&clients.internet, head, url, head.headers.clone(), body).await
}

fn join(base: &Url, target: &str) -> Result<Url, String> {
    let root = base.as_str().trim_end_matches('/');
    let url = Url::parse(&format!("{root}{target}"))
        .map_err(|error| format!("the target does not join the service's base: {error}"))?;
    let prefix = base.path().trim_end_matches('/');
    let path = url.path();
    let under = prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"));
    let inside = url.origin() == base.origin() && under;
    if !inside {
        return Err(format!("{target} leaves the service's base"));
    }
    Ok(url)
}

async fn to_service(
    clients: &Clients,
    config: &Config,
    caller: &Caller<'_>,
    name: &str,
    head: &Head,
    body: Vec<u8>,
) -> Result<Response, String> {
    let Some(Upstream { base, secrets }) = config.services.get(name) else {
        return Err(format!("this node does not serve {name}"));
    };
    head.check_path().map_err(|refusal| refusal.sentence)?;
    let url = join(base, &head.target)?;
    let clash = head
        .headers
        .iter()
        .find(|(name, _)| secrets.iter().any(|(secret, _)| secret == name));
    if let Some((header, _)) = clash {
        return Err(format!("{header} is this node's to set for {name}"));
    }
    let headers = head
        .headers
        .iter()
        .chain(secrets)
        .cloned()
        .chain(caller.headers())
        .collect();
    send(&clients.services, head, url, headers, body).await
}

async fn send(
    client: &reqwest::Client,
    head: &Head,
    url: Url,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<Response, String> {
    let method = reqwest::Method::from_bytes(head.method.as_str().as_bytes())
        .map_err(|error| error.to_string())?;
    let mut request = client.request(method, url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("the target was not reached: {error}"))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("the answer broke off: {error}"))?
        .to_vec();
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests;
