use gateway as wire;
use std::fs;

fn fixture(name: &str) -> Vec<u8> {
    let bytes = fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    if name.ends_with(".json") {
        bytes.strip_suffix(b"\n").unwrap_or(&bytes).to_vec()
    } else {
        bytes
    }
}

fn hex_fixture(name: &str) -> Vec<u8> {
    let text = String::from_utf8(fixture(name)).unwrap();
    text.trim()
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |byte| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("non-hex fixture byte"),
            };
            nibble(pair[0]) << 4 | nibble(pair[1])
        })
        .collect()
}

fn request_head() -> wire::ProxyRequestHead {
    wire::ProxyRequestHead {
        operator: false,
        account_id: 7,
        name: wire::RouteName::named("api"),
        revision: 3,
        method: wire::RouteMethod::Post,
        path_and_query: "/v1/items?x=1".into(),
        headers: vec![wire::ProxyHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        upgrade: false,
        user_pop: None,
    }
}

fn request_head_with_pop() -> wire::ProxyRequestHead {
    wire::ProxyRequestHead {
        user_pop: Some(wire::UserPop {
            key: vec![0x44; 32],
            ts: 42,
            sig: vec![0x55; 66],
        }),
        ..request_head()
    }
}

fn route_statement() -> wire::RouteStatement {
    wire::RouteStatement {
        chain_id: "dognet#1".into(),
        account_id: 7,
        name: wire::RouteName::named("api"),
        publisher_node: vec![0x11; wire::NODE_KEY_BYTES],
        revision: 3,
        route: Some(wire::RouteDefinition {
            target: wire::RouteTarget::LoopbackHttp,
            policy: wire::RoutePolicy {
                audience: wire::RouteAudience::Owner,
                methods: vec![wire::RouteMethod::Post],
                max_request_bytes: Some(1024),
                max_response_bytes: 4096,
                allow_authorization: true,
                allow_upgrade: false,
            },
        }),
    }
}

fn route_record() -> wire::RouteRecord {
    wire::RouteRecord {
        statement: route_statement(),
        authorization: wire::MemberAuthorization {
            signer: vec![0x22; 32],
            signature: vec![0x33; 64],
        },
    }
}

fn response_head() -> wire::ProxyResponseHead {
    wire::ProxyResponseHead {
        status: 201,
        headers: vec![wire::ProxyHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
    }
}

#[test]
fn committed_json_and_binary_goldens_match_current_codecs() {
    let msg = wire::GatewayMsg::SetHandle { handle: None };
    assert_eq!(wire::encode_msg(&msg), fixture("msg_set_handle_none.json"));
    assert_eq!(
        wire::decode_msg(&fixture("msg_set_handle_none.json")).unwrap(),
        msg
    );

    let query = wire::GatewayQuery::Registrations { from: 2, limit: 5 };
    assert_eq!(
        wire::encode_query(&query),
        fixture("query_registrations_page.json")
    );
    assert_eq!(
        wire::decode_query(&fixture("query_registrations_page.json")).unwrap(),
        query
    );

    let resolve = wire::GatewayQuery::Resolve {
        name: duckdns::DuckDnsName {
            handle: "alice".into(),
        },
    };
    assert_eq!(wire::encode_query(&resolve), fixture("query_resolve.json"));
    assert_eq!(
        wire::decode_query(&fixture("query_resolve.json")).unwrap(),
        resolve
    );

    let get = wire::GatewayQuery::Get {
        account_id: 7,
        name: wire::RouteName::named("api"),
    };
    assert_eq!(wire::encode_query(&get), fixture("query_get.json"));
    assert_eq!(wire::decode_query(&fixture("query_get.json")).unwrap(), get);

    let reply = wire::GatewayReply::Resolved(None);
    assert_eq!(
        wire::encode_reply(&reply),
        fixture("reply_resolved_none.json")
    );
    assert_eq!(
        wire::decode_reply(&fixture("reply_resolved_none.json")).unwrap(),
        reply
    );

    let resolved = wire::GatewayReply::Resolved(Some(duckdns::ResolvedAccount { account_id: 7 }));
    assert_eq!(
        wire::encode_reply(&resolved),
        fixture("reply_resolved_some.json")
    );
    assert_eq!(
        wire::decode_reply(&fixture("reply_resolved_some.json")).unwrap(),
        resolved
    );

    let route = wire::GatewayReply::Route(Box::new(Some(route_record())));
    assert_eq!(wire::encode_reply(&route), fixture("reply_route.json"));
    assert_eq!(
        wire::decode_reply(&fixture("reply_route.json")).unwrap(),
        route
    );

    let request = request_head();
    assert_eq!(
        wire::encode_proxy_request_head(&request).unwrap(),
        fixture("proxy_request_head.json")
    );
    assert_eq!(
        wire::decode_proxy_request_head(&fixture("proxy_request_head.json")).unwrap(),
        request
    );

    let request_with_pop = request_head_with_pop();
    assert_eq!(
        wire::encode_proxy_request_head(&request_with_pop).unwrap(),
        fixture("proxy_request_head_user_pop.json")
    );
    assert_eq!(
        wire::decode_proxy_request_head(&fixture("proxy_request_head_user_pop.json")).unwrap(),
        request_with_pop
    );

    let response = wire::ProxyFrame::ResponseHead(response_head());
    let response_bytes = hex_fixture("frame_response_head.hex");
    assert_eq!(wire::encode_frame(&response).unwrap(), response_bytes);
    assert_eq!(
        wire::decode_frame(&response_bytes).unwrap(),
        (response, response_bytes.len())
    );

    let frame = wire::ProxyFrame::Failure(wire::ProxyFailure {
        kind: wire::FailureKind::TooLarge,
        detail: "body limit".into(),
    });
    let frame_bytes = hex_fixture("frame_failure.hex");
    assert_eq!(wire::encode_frame(&frame).unwrap(), frame_bytes);
    assert_eq!(
        wire::decode_frame(&frame_bytes).unwrap(),
        (frame, frame_bytes.len())
    );

    let statement = route_statement();
    assert_eq!(
        wire::route_signing_preimage(&statement).unwrap(),
        hex_fixture("route_signing_preimage.hex")
    );

    let digest = wire::body_digest(b"gateway request body");
    assert_eq!(digest.as_slice(), hex_fixture("body_digest.hex"));
    assert_eq!(
        wire::caller_pop_preimage(
            &[0x22; wire::NODE_KEY_BYTES],
            &request_with_pop,
            &digest,
            42
        ),
        hex_fixture("caller_pop_preimage.hex")
    );

    for (bytes, fixture_name) in [
        (wire::GATEWAY_ROUTE_NS, "gateway_route_namespace.hex"),
        (
            wire::GATEWAY_CREDENTIAL_NS,
            "gateway_credential_namespace.hex",
        ),
        (wire::GATEWAY_CALLER_NS, "gateway_caller_namespace.hex"),
        (wire::PROXY_FLOW_DOMAIN, "proxy_flow_domain.hex"),
    ] {
        assert_eq!(bytes, hex_fixture(fixture_name));
    }

    assert!(wire::decode_proxy_request_head(&fixture("proxy_unknown_field.json")).is_err());
}
