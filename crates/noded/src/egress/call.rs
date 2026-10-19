//! One egress request answered by this node: a relay, a public `http` request, or a registered
//! service's request at its configured base, its reply whole or as a stream of events. Used by the
//! egress worker in `mod.rs`.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use abi::role::egress::http::{Head, Response};
use abi::role::egress::{Answer, Delivery, HTTP, Request, StreamEnd};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use url::Url;

use crate::egress::config::{Config, Reach, Refused, Upstream};
use crate::egress::desk::Called;
use crate::egress::stream::Splitter;
use crate::wire::Reached;

const EVENT_STREAM: &str = "text/event-stream";

pub struct Clients {
    internet: reqwest::Client,
    services: reqwest::Client,
}

impl Clients {
    pub fn new(reach: Reach) -> Result<Clients, Refused> {
        let internet = match reach {
            Reach::Public => builder().dns_resolver(Arc::new(PublicOnly)),
            Reach::Private => builder(),
        };
        let built = |builder: reqwest::ClientBuilder| builder.build().map_err(Refused::Client);
        Ok(Clients {
            internet: built(internet)?,
            services: built(builder())?,
        })
    }

    pub async fn reach(&self, base: &Url, wait: Duration) -> Reached {
        let error = match self.services.get(base.clone()).timeout(wait).send().await {
            Ok(response) => return Reached::Answered(response.status().as_u16()),
            Err(error) => error,
        };
        let failed = match error.is_timeout() {
            true => format!(
                "{base} did not answer within {} ms, the wait egress gives a request",
                wait.as_millis()
            ),
            false => unreached(base, error),
        };
        Reached::Failed(failed)
    }
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .no_proxy()
}

struct PublicOnly;

#[derive(Debug, thiserror::Error)]
#[error("{0} has no public address")]
struct NotPublic(String);

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|address| public(address.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(NotPublic(host)) as Box<dyn std::error::Error + Send + Sync>);
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

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "te",
    "transfer-encoding",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
];

fn end_to_end(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    let named: Vec<String> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .collect();
    headers
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            let hop = HOP_BY_HOP.contains(&name.as_str()) || named.contains(&name);
            !hop
        })
        .collect()
}

fn unreached(url: &Url, error: reqwest::Error) -> String {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut causes = Vec::new();
    while let Some(cause) = source {
        if cause.downcast_ref::<NotPublic>().is_some() {
            return format!("{url} is not a public address");
        }
        causes.push(cause.to_string());
        source = cause.source();
    }
    format!("the target was not reached: {}", causes.join(": "))
}

pub struct Caller<'a> {
    pub chain: &'a str,
    pub program: &'a str,
    pub id: u64,
    pub lent: &'a [(String, String)],
}

impl Caller<'_> {
    fn headers(&self) -> impl Iterator<Item = (String, String)> + '_ {
        let own = [
            ("ducktape-chain".to_owned(), self.chain.to_owned()),
            ("ducktape-program".to_owned(), self.program.to_owned()),
            ("ducktape-request".to_owned(), self.id.to_string()),
        ];
        own.into_iter().chain(self.lent.iter().cloned())
    }
}

pub async fn answer(
    clients: &Clients,
    config: &Config,
    caller: &Caller<'_>,
    request: &Request,
    delivery: &Delivery,
    body: Vec<u8>,
    event: impl FnMut(Vec<u8>),
) -> Called {
    let sent = match request {
        Request::Relay { .. } => return Called::Whole(relay(config)),
        Request::Http { service, head, .. } => match service.as_str() {
            HTTP => to_internet(clients, config, head, body).await,
            named => to_service(clients, config, caller, named, head, body).await,
        },
    };
    let response = match sent {
        Ok(response) => response,
        Err(reason) => return Called::Whole(Answer::Failed(reason)),
    };
    let status = response.status().as_u16();
    let headers = end_to_end(received(&response));
    let streams = headers.iter().any(event_stream);
    match (delivery, streams) {
        (Delivery::Events { .. }, true) => {
            let complete = StreamEnd::Complete { status, headers };
            Called::End(stream(response, complete, event).await)
        }
        (Delivery::Events { .. }, false) | (Delivery::Whole, _) => {
            let whole = match response.bytes().await {
                Ok(body) => Answer::Output(abi::encode(&Response {
                    status,
                    headers,
                    body: body.to_vec(),
                })),
                Err(error) => Answer::Failed(format!("the answer broke off: {error}")),
            };
            Called::Whole(whole)
        }
    }
}

fn event_stream((name, value): &(String, String)) -> bool {
    let media = value.split(';').next().unwrap_or_default().trim();
    name.eq_ignore_ascii_case("content-type") && media.eq_ignore_ascii_case(EVENT_STREAM)
}

async fn stream(
    mut response: reqwest::Response,
    complete: StreamEnd,
    mut event: impl FnMut(Vec<u8>),
) -> StreamEnd {
    let mut splitter = Splitter::default();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                splitter.finish().into_iter().for_each(event);
                return complete;
            }
            Err(_) => {
                return StreamEnd::Cut {
                    reason: "stream_broken".to_owned(),
                };
            }
        };
        splitter.push(&chunk).into_iter().for_each(&mut event);
    }
}

fn received(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
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
) -> Result<reqwest::Response, String> {
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
) -> Result<reqwest::Response, String> {
    let Some(Upstream { base, secrets, .. }) = config.services.get(name) else {
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
) -> Result<reqwest::Response, String> {
    let method = reqwest::Method::from_bytes(head.method.as_str().as_bytes())
        .map_err(|error| error.to_string())?;
    let mut request = client.request(method, url.clone());
    for (name, value) in end_to_end(headers) {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }
    request.send().await.map_err(|error| unreached(&url, error))
}

#[cfg(test)]
mod tests;
