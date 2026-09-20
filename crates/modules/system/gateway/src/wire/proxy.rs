//! Bounded request contract for the gateway reverse proxy.
//!
//! The stream hello carries only [`ProxyRequestHead`], which declares no
//! length: the caller writes its body as `ProxyFrame::BodyChunk` frames ended
//! by `ProxyFrame::End`, exactly as the response direction already did, and
//! the publisher counts those bytes against the route's own
//! [`request_body_allowance`] as they arrive. Nothing holds a request body to
//! measure it, so what a route accepts is a policy and never a buffer. The
//! publisher re-resolves the route and caller account before touching DuckFS
//! or loopback, so this is not a raw filesystem, socket, or reverse-proxy
//! primitive.

use serde::{Deserialize, Serialize};

use crate::{RouteAudience, RouteMethod, RouteName, RouteRecord, validate_account_number};

pub const PROXY_FLOW_DOMAIN: &[u8] = b"ducktape-gateway-proxy-v1";
pub const PROXY_INTENT: u8 = 1;
pub const MAX_PROXY_HEAD_BYTES: usize = 8192;
pub const MAX_PATH_AND_QUERY_BYTES: usize = 2048;
pub const MAX_HEADERS: usize = 32;
pub const MAX_HEADER_NAME_BYTES: usize = 64;
pub const MAX_HEADER_VALUE_BYTES: usize = 4096;
pub const MAX_HEADER_BYTES: usize = 16384;

/// Request headers stripped before the publisher forwards to its upstream:
/// hop-by-hop, forwarding/identity spoofables, and every `x-duck-*` (the proxy
/// alone mints those). Compared case-insensitively; the wire also rejects
/// non-lowercase and `x-duck-*` names at decode, so this is defense in depth
/// against a raw peer that never went through decode.
pub fn header_forwardable(name: &str) -> bool {
    const DENY: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        // The publisher sets these itself; a forwarded copy would duplicate or
        // fight the proxy's value.
        "accept-encoding",
        "content-length",
        "user-agent",
        "host",
        "origin",
        "referer",
        "via",
        "forwarded",
        "x-forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
        "true-client-ip",
        "cf-connecting-ip",
        "client-ip",
    ];
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("x-duck-") || lower.starts_with("x_duck_") {
        return false;
    }
    !DENY.contains(&lower.as_str())
}

/// Response headers a publisher may return. Response stays fail-closed
/// (allowlist) because a response header can carry policy weight (cookies,
/// caching, redirects); request headers are the denylisted direction.
pub const ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "cache-control",
    "content-disposition",
    "content-language",
    "content-type",
    "etag",
    "last-modified",
    "location",
    "retry-after",
    "set-cookie",
    "vary",
];

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProxyHeader {
    /// Canonical lowercase ASCII. Cookie, Set-Cookie, forwarding headers, CORS,
    /// and security headers are absent from the allowlists by construction.
    pub name: String,
    pub value: String,
}

/// A caller's proof of possession of a user key for this one request: `sig`
/// is `key`'s scheme-owned proof over [`crate::caller_pop_preimage`] under
/// [`crate::GATEWAY_CALLER_NS`]. The serving node resolves `key` to its
/// account through Identity; a head without one carries no account.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UserPop {
    pub key: Vec<u8>,
    pub ts: u64,
    pub sig: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProxyRequestHead {
    /// The authenticated calling node attests that its operator authorized this
    /// exchange. This is not authority on the publisher: services still apply
    /// their existing local/peer admission. Public ingress never trusts this
    /// client-supplied field; only the operator-authenticated node door sets it.
    #[serde(default)]
    pub operator: bool,
    pub account_id: u64,
    pub name: RouteName,
    pub revision: u64,
    pub method: RouteMethod,
    /// Strict HTTP origin-form (`/path?query`), never an absolute URI.
    pub path_and_query: String,
    /// Strictly name-sorted, unique, allowlisted request headers.
    pub headers: Vec<ProxyHeader>,
    /// Request a WebSocket upgrade (GET, no body) on a route signed
    /// `allow_upgrade`.
    pub upgrade: bool,
    /// The caller's user-key proof, or none (an account-less caller).
    pub user_pop: Option<UserPop>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProxyResponseHead {
    pub status: u16,
    /// Strictly name-sorted, unique, allowlisted response headers.
    pub headers: Vec<ProxyHeader>,
}

pub fn encode_proxy_request_head(head: &ProxyRequestHead) -> Result<Vec<u8>, String> {
    validate_proxy_request_head(head)?;
    let bytes = serde_json::to_vec(head).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_PROXY_HEAD_BYTES {
        return Err(format!("proxy: head exceeds {MAX_PROXY_HEAD_BYTES} bytes"));
    }
    Ok(bytes)
}

pub fn decode_proxy_request_head(bytes: &[u8]) -> Result<ProxyRequestHead, String> {
    if bytes.len() > MAX_PROXY_HEAD_BYTES {
        return Err(format!("proxy: head exceeds {MAX_PROXY_HEAD_BYTES} bytes"));
    }
    let head: ProxyRequestHead =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    validate_proxy_request_head(&head)?;
    Ok(head)
}

pub fn validate_proxy_request_head(head: &ProxyRequestHead) -> Result<(), String> {
    validate_account_number(head.account_id)?;
    head.name.validate()?;
    if head.revision == 0 {
        return Err("proxy: revision starts at 1".into());
    }
    validate_origin_form(&head.path_and_query)?;
    validate_headers(&head.headers, "request")?;
    // A head no longer declares a length — the body streams after it, and the
    // serving side counts it against the route's own cap as it arrives. What
    // is checkable here is the METHOD: an upgrade and a GET/HEAD carry no body
    // at all, so the first body frame on one of those is the refusal, and
    // `request_body_allowance` is what says so.
    if head.upgrade && head.method != RouteMethod::Get {
        return Err("proxy: a WebSocket upgrade must be a bodyless GET".into());
    }
    Ok(())
}

pub fn validate_response_head(head: &ProxyResponseHead) -> Result<(), String> {
    if !(200..=599).contains(&head.status) {
        return Err("proxy: invalid upstream status".into());
    }
    validate_headers(&head.headers, "response")?;
    // Responses stay fail-closed on an allowlist (see `ALLOWED_RESPONSE_HEADERS`).
    for header in &head.headers {
        if !ALLOWED_RESPONSE_HEADERS.contains(&header.name.as_str()) {
            return Err(format!(
                "proxy: disallowed response header {:?}",
                header.name
            ));
        }
    }
    if let Some(location) = header_value(&head.headers, "location") {
        validate_safe_location(location)?;
    }
    Ok(())
}

pub fn validate_origin_form(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.len() > MAX_PATH_AND_QUERY_BYTES
        || value.contains(['\r', '\n', '\\', '#'])
        || !value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
    {
        return Err("proxy: invalid origin-form path/query".into());
    }
    // URL clients remove these segments before sending. Refuse them so the
    // path verified by the publisher is the path the upstream receives.
    let path = value.split('?').next().unwrap_or(value);
    let normalized_by_clients = path.split('/').any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
        )
    });
    if normalized_by_clients {
        return Err("proxy: path contains a URL-normalized segment".into());
    }
    Ok(())
}

pub fn validate_safe_location(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.len() > MAX_PATH_AND_QUERY_BYTES
        || value.contains(['\r', '\n', '\\'])
        || !value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
    {
        return Err("proxy: unsafe redirect location".into());
    }
    Ok(())
}

pub fn validate_headers(headers: &[ProxyHeader], kind: &str) -> Result<(), String> {
    if headers.len() > MAX_HEADERS {
        return Err(format!(
            "proxy: too many {kind} headers (max {MAX_HEADERS})"
        ));
    }
    let mut previous: Option<&str> = None;
    let mut total = 0usize;
    for header in headers {
        // Names are strict lowercase HTTP tokens. Rejecting non-lowercase and
        // underscore names here is what stops an `X-Duck-Caller-Account` /
        // `x_duck_caller_account` spoof from ever reaching the forward step —
        // the proxy alone mints the authenticated `x-duck-*` headers.
        let first_bad_byte = header
            .name
            .bytes()
            .position(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));
        let malformed_name = header.name.is_empty()
            || header.name.len() > MAX_HEADER_NAME_BYTES
            || first_bad_byte.is_some();
        if malformed_name {
            // The name is remote input and need not be ASCII or bounded, so it
            // is described, never echoed: an echoed name lands in a failure
            // detail that gets byte-bounded, and in the node's log ring.
            let offset = first_bad_byte.map_or_else(|| "none".to_string(), |at| at.to_string());
            return Err(format!(
                "proxy: malformed {kind} header name (len {}, first invalid byte at {offset})",
                header.name.len()
            ));
        }
        if header.name.starts_with("x-duck-") {
            return Err(format!(
                "proxy: {kind} header {:?} spoofs a proxy-minted header",
                header.name
            ));
        }
        // Sorted, and unique except for `set-cookie` — the one header HTTP
        // genuinely repeats (each cookie needs its own line; folding them into
        // one value is illegal). Repeats must still be adjacent, so the list
        // stays canonical and a receiver can group by name in one pass.
        let repeatable = kind == "response" && header.name == "set-cookie";
        let out_of_order = previous.is_some_and(|old| match old.cmp(header.name.as_str()) {
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => !repeatable,
            std::cmp::Ordering::Greater => true,
        });
        if out_of_order {
            return Err(format!(
                "proxy: {kind} headers must be sorted, and unique except set-cookie"
            ));
        }
        previous = Some(&header.name);
        if header.value.is_empty()
            || header.value.len() > MAX_HEADER_VALUE_BYTES
            || !header.value.is_ascii()
            || header.value.bytes().any(|byte| byte < b' ' || byte == 0x7f)
        {
            return Err(format!(
                "proxy: invalid value for {kind} header {:?}",
                header.name
            ));
        }
        total = total
            .checked_add(header.name.len() + header.value.len())
            .ok_or_else(|| "proxy: header size overflow".to_string())?;
    }
    if total > MAX_HEADER_BYTES {
        return Err(format!(
            "proxy: {kind} headers exceed {MAX_HEADER_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn header_value<'a>(headers: &'a [ProxyHeader], name: &str) -> Option<&'a str> {
    headers
        .binary_search_by(|header| header.name.as_str().cmp(name))
        .ok()
        .map(|index| headers[index].value.as_str())
}

/// `caller` is the account the request's user proof resolved to, or `None`
/// for a mesh peer that proved no user key. `Network` admits either.
pub fn audience_allows(audience: &RouteAudience, owner: u64, caller: Option<u64>) -> bool {
    match audience {
        RouteAudience::Owner => caller == Some(owner),
        RouteAudience::Network => true,
        RouteAudience::Accounts { account_ids } => {
            caller.is_some_and(|caller| account_ids.binary_search(&caller).is_ok())
        }
    }
}

/// Validate invocation policy against the current route. This is run at the
/// consumer before opening a stream and again at the publisher after resolving
/// its own finalized state.
pub fn request_matches_record(head: &ProxyRequestHead, record: &RouteRecord) -> bool {
    let statement = &record.statement;
    let Some(route) = &statement.route else {
        return false;
    };
    statement.account_id == head.account_id
        && statement.name == head.name
        && statement.revision == head.revision
        && route.policy.methods.binary_search(&head.method).is_ok()
        && (route.policy.allow_authorization
            || header_value(&head.headers, "authorization").is_none())
}

/// How many body bytes this request may still send — `None` for no limit.
///
/// The head carries no length any more, so this is the whole request-size
/// decision and it is made ONCE, from the signed policy, before a byte is
/// read. A method that carries no body gets `Some(0)`, which refuses the
/// first frame; a route with no cap gets `None` and the transport decides
/// nothing.
pub fn request_body_allowance(head: &ProxyRequestHead, record: &RouteRecord) -> Option<u64> {
    let carries_a_body = head.method.permits_body() && !head.upgrade;
    if !carries_a_body {
        return Some(0);
    }
    record
        .statement
        .route
        .as_ref()
        .and_then(|route| route.policy.max_request_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemberAuthorization, RouteDefinition, RoutePolicy, RouteStatement, RouteTarget};

    /// The name is remote input; the refusal describes it and never carries
    /// its bytes, so the detail downstream stays ASCII and bounded.
    #[test]
    fn signed_paths_are_not_normalized_by_url_clients() {
        for path in [
            "/a/../b",
            "/a/./b",
            "/%2e/b",
            "/.%2E/b",
            "/%2e./b",
            "/%2e%2E/b",
        ] {
            assert!(validate_origin_form(path).is_err(), "accepted {path}");
        }
        for path in ["/a/b?next=../c", "/v1.0/item", "/a%20b", "/.../item"] {
            assert!(validate_origin_form(path).is_ok(), "refused {path}");
        }
    }

    #[test]
    fn a_malformed_header_name_is_described_not_echoed() {
        let headers = vec![ProxyHeader {
            name: "€".repeat(200),
            value: "v".into(),
        }];
        let error = validate_headers(&headers, "request").expect_err("non-token name is refused");
        assert!(error.is_ascii(), "the refusal echoed peer bytes: {error}");
        assert!(!error.contains('€'));
        assert!(error.contains("len 600"), "names the length: {error}");
        assert!(error.contains("first invalid byte at 0"), "{error}");
    }

    fn record() -> RouteRecord {
        RouteRecord {
            statement: RouteStatement {
                chain_id: "test".into(),
                account_id: 1,
                name: RouteName::named("api"),
                publisher_node: vec![2; 32],
                revision: 7,
                route: Some(RouteDefinition {
                    target: RouteTarget::LoopbackHttp,
                    policy: RoutePolicy {
                        audience: RouteAudience::Network,
                        methods: vec![RouteMethod::Get, RouteMethod::Post],
                        max_request_bytes: Some(1024),
                        max_response_bytes: 4096,
                        allow_authorization: false,
                        allow_upgrade: false,
                    },
                }),
            },
            authorization: MemberAuthorization {
                signer: vec![3; 32],
                signature: vec![4; 64],
            },
        }
    }

    fn head() -> ProxyRequestHead {
        ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: RouteName::named("api"),
            revision: 7,
            method: RouteMethod::Post,
            path_and_query: "/v1/items".into(),
            headers: vec![ProxyHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
            upgrade: false,
            user_pop: Some(UserPop {
                key: vec![5; 32],
                ts: 1_700_000_000,
                sig: vec![6; 64],
            }),
        }
    }

    #[test]
    fn invocation_is_record_method_revision_and_header_scoped() {
        let head = head();
        let encoded = encode_proxy_request_head(&head).unwrap();
        assert_eq!(decode_proxy_request_head(&encoded).unwrap(), head);
        assert!(request_matches_record(&head, &record()));

        let mut stale = head.clone();
        stale.revision -= 1;
        assert!(!request_matches_record(&stale, &record()));
        let mut forbidden = head.clone();
        forbidden.method = RouteMethod::Delete;
        assert!(!request_matches_record(&forbidden, &record()));
        let mut ambient = head;
        ambient.headers = vec![ProxyHeader {
            name: "authorization".into(),
            value: "Bearer secret".into(),
        }];
        assert!(!request_matches_record(&ambient, &record()));
    }

    #[test]
    fn request_head_requires_the_upgrade_verdict() {
        let mut value = serde_json::to_value(ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: RouteName::named("api"),
            revision: 7,
            method: RouteMethod::Get,
            path_and_query: "/".into(),
            headers: Vec::new(),
            upgrade: false,
            user_pop: None,
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("upgrade");
        assert!(decode_proxy_request_head(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn a_head_without_a_user_pop_decodes_as_an_account_less_caller() {
        let mut value = serde_json::to_value(ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: RouteName::named("api"),
            revision: 7,
            method: RouteMethod::Get,
            path_and_query: "/".into(),
            headers: Vec::new(),
            upgrade: false,
            user_pop: None,
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("user_pop");
        let head = decode_proxy_request_head(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(head.user_pop, None);
    }

    #[test]
    fn absolute_urls_and_smuggling_fail_closed() {
        for path in [
            "https://evil.test",
            "//evil.test/x",
            "/x\r\nHost: evil",
            "/x#fragment",
            "/\\evil.test",
        ] {
            assert!(validate_origin_form(path).is_err(), "accepted {path:?}");
        }
    }

    fn one_header(name: &str) -> Vec<ProxyHeader> {
        vec![ProxyHeader {
            name: name.into(),
            value: "x".into(),
        }]
    }

    #[test]
    fn decode_rejects_caller_account_spoofs() {
        // Any x-duck-* is rejected at decode; the proxy alone mints those.
        assert!(validate_headers(&one_header("x-duck-caller-account"), "request").is_err());
        // Non-lowercase / underscore names never reach the denylist.
        assert!(validate_headers(&one_header("X-Duck-Caller-Account"), "request").is_err());
        assert!(validate_headers(&one_header("x_duck_caller_account"), "request").is_err());
        // Ordinary credential/content headers now pass decode (they flow e2e).
        assert!(validate_headers(&one_header("cookie"), "request").is_ok());
        assert!(validate_headers(&one_header("authorization"), "request").is_ok());
        assert!(validate_headers(&one_header("content-type"), "request").is_ok());
    }

    #[test]
    fn forward_denylist_strips_hop_by_hop_and_identity() {
        for name in [
            "connection",
            "transfer-encoding",
            "upgrade",
            "host",
            "origin",
            "referer",
            "via",
            "x-forwarded-for",
            "x-real-ip",
            "cf-connecting-ip",
            "x-duck-caller-account",
        ] {
            assert!(!header_forwardable(name), "should have stripped {name}");
        }
        for name in [
            "cookie",
            "authorization",
            "content-type",
            "accept",
            "if-none-match",
        ] {
            assert!(header_forwardable(name), "should have forwarded {name}");
        }
    }

    #[test]
    fn response_headers_stay_allowlisted() {
        let ok = ProxyResponseHead {
            status: 200,
            headers: vec![ProxyHeader {
                name: "set-cookie".into(),
                value: "s=1".into(),
            }],
        };
        assert!(validate_response_head(&ok).is_ok());
        let bad = ProxyResponseHead {
            status: 200,
            headers: vec![ProxyHeader {
                name: "x-frame-options".into(),
                value: "DENY".into(),
            }],
        };
        assert!(validate_response_head(&bad).is_err());
    }

    #[test]
    fn audience_is_separate_from_global_name_resolution() {
        let owner = 1;
        let bob = 2;
        assert!(audience_allows(&RouteAudience::Owner, owner, Some(owner)));
        assert!(!audience_allows(&RouteAudience::Owner, owner, Some(bob)));
        assert!(!audience_allows(&RouteAudience::Owner, owner, None));
        // any mesh peer, proven account or not.
        assert!(audience_allows(&RouteAudience::Network, owner, Some(bob)));
        assert!(audience_allows(&RouteAudience::Network, owner, None));
        let explicit = RouteAudience::Accounts {
            account_ids: vec![bob],
        };
        assert!(audience_allows(&explicit, owner, Some(bob)));
        assert!(!audience_allows(&explicit, owner, Some(owner)));
        assert!(!audience_allows(&explicit, owner, None));
    }

    #[test]
    fn caller_cannot_forge_the_zero_account() {
        let head = ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: RouteName::apex(),
            revision: 1,
            method: RouteMethod::Post,
            path_and_query: "/".into(),
            headers: vec![],
            upgrade: false,
            user_pop: None,
        };
        assert!(validate_proxy_request_head(&head).is_ok());
        let mut json = serde_json::to_value(&head).unwrap();
        json["account_id"] = serde_json::json!(0);
        assert!(decode_proxy_request_head(&serde_json::to_vec(&json).unwrap()).is_err());
    }

    /// A head cannot declare a size any more, so the size question is settled
    /// from the SIGNED policy instead: a bodyless method gets no allowance at
    /// all, a capped route gets its cap, and a route that declined to cap
    /// itself gets none — the serving side counts as it reads.
    #[test]
    fn the_body_allowance_comes_from_the_signed_policy_not_the_caller() {
        let record = record();
        let post = ProxyRequestHead {
            method: RouteMethod::Post,
            ..head()
        };
        assert_eq!(request_body_allowance(&post, &record), Some(1024));

        let get = ProxyRequestHead {
            method: RouteMethod::Get,
            ..head()
        };
        assert_eq!(request_body_allowance(&get, &record), Some(0));

        let upgrade = ProxyRequestHead {
            method: RouteMethod::Get,
            upgrade: true,
            ..head()
        };
        assert_eq!(request_body_allowance(&upgrade, &record), Some(0));

        let mut uncapped = record;
        uncapped
            .statement
            .route
            .as_mut()
            .unwrap()
            .policy
            .max_request_bytes = None;
        assert_eq!(request_body_allowance(&post, &uncapped), None);
    }
}
