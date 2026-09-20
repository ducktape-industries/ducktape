// Core-owned Gateway producer goldens. Route/proxy objects use JSON; caller POP
// preimages use the SDK's length-prefixed little-endian signing layout.
#[path = "fixtures/producer_goldens.rs"]
mod fixtures;

use gateway::*;

fn decode_hex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn golden(name: &str, actual: &[u8], expected: &str) -> Vec<u8> {
    let expected = decode_hex(expected);
    assert_eq!(actual, expected.as_slice(), "{name}");
    expected
}

fn record(name: RouteName) -> RouteRecord {
    RouteRecord {
        statement: RouteStatement {
            chain_id: "dognet".into(),
            account_id: 7,
            name,
            publisher_node: vec![0x11; 32],
            revision: 3,
            route: Some(RouteDefinition {
                target: RouteTarget::LoopbackHttp,
                policy: RoutePolicy {
                    audience: RouteAudience::Owner,
                    methods: vec![
                        RouteMethod::Get,
                        RouteMethod::Head,
                        RouteMethod::Post,
                        RouteMethod::Put,
                        RouteMethod::Patch,
                        RouteMethod::Delete,
                    ],
                    max_request_bytes: Some(4096),
                    max_response_bytes: 8192,
                    allow_authorization: true,
                    allow_upgrade: false,
                },
            }),
        },
        authorization: MemberAuthorization {
            signer: vec![0x22; 33],
            signature: vec![0xaa, 0xbb, 0xcc],
        },
    }
}

fn head() -> ProxyRequestHead {
    ProxyRequestHead {
        operator: true,
        account_id: 7,
        name: RouteName::named("api"),
        revision: 3,
        method: RouteMethod::Post,
        path_and_query: "/v1/items?x=1".into(),
        headers: vec![
            ProxyHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            },
            ProxyHeader {
                name: "x-request-id".into(),
                value: "req-1".into(),
            },
        ],
        upgrade: false,
        user_pop: Some(UserPop {
            key: vec![0x22; 33],
            ts: 55,
            sig: vec![0xaa, 0xbb],
        }),
    }
}

#[test]
fn route_queries_and_reply_match_committed_bytes_and_decode() {
    let apex = GatewayQuery::Get {
        account_id: 7,
        name: RouteName::apex(),
    };
    let named = GatewayQuery::Get {
        account_id: 7,
        name: RouteName::named("api"),
    };
    for (name, query, expected) in [
        ("get_apex", apex, fixtures::QUERY_GET_APEX),
        ("get_named", named, fixtures::QUERY_GET_NAMED),
    ] {
        let bytes = golden(name, &encode_query(&query), expected);
        assert_eq!(decode_query(&bytes).unwrap(), query);
    }
    let reply = GatewayReply::Route(Box::new(Some(record(RouteName::named("api")))));
    let bytes = golden("route_reply", &encode_reply(&reply), fixtures::REPLY_ROUTE);
    assert_eq!(decode_reply(&bytes).unwrap(), reply);
}

#[test]
fn proxy_head_and_caller_preimages_match_committed_bytes() {
    let head = head();
    let head_bytes = encode_proxy_request_head(&head).expect("valid proxy head");
    let expected_head = golden(
        "proxy_request_head",
        &head_bytes,
        fixtures::PROXY_REQUEST_HEAD,
    );
    assert_eq!(decode_proxy_request_head(&expected_head).unwrap(), head);

    let empty = body_digest(b"");
    let nonempty = body_digest(b"body");
    assert_eq!(
        empty.as_slice(),
        decode_hex(fixtures::BODY_DIGEST_EMPTY).as_slice()
    );
    assert_eq!(
        nonempty.as_slice(),
        decode_hex(fixtures::BODY_DIGEST_NONEMPTY).as_slice()
    );
    assert_eq!(
        caller_pop_preimage(&[0x11; 32], &head, &empty, 55),
        decode_hex(fixtures::CALLER_POP_PREIMAGE_EMPTY),
    );
    assert_eq!(
        caller_pop_preimage(&[0x11; 32], &head, &nonempty, 55),
        decode_hex(fixtures::CALLER_POP_PREIMAGE_NONEMPTY),
    );
}
