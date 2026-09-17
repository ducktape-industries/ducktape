use gateway::{
    GatewayReply, RouteAudience, RouteDefinition, RouteMethod, RouteName, RoutePolicy,
    RouteStatement, RouteSummary, RouteTarget, route_signing_preimage, validate_policy,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn statement() -> RouteStatement {
    RouteStatement {
        chain_id: "test".into(),
        account_id: 7,
        name: RouteName::named("api"),
        publisher_node: vec![3; 32],
        revision: 7,
        route: Some(RouteDefinition {
            target: RouteTarget::LoopbackHttp,
            policy: RoutePolicy {
                audience: RouteAudience::Network,
                methods: vec![RouteMethod::Get, RouteMethod::Head, RouteMethod::Post],
                max_request_bytes: Some(1024),
                max_response_bytes: 4096,
                allow_authorization: false,
                allow_upgrade: true,
            },
        }),
    }
}

#[test]
fn signing_preimage_has_a_cross_language_fixed_vector() {
    let encoded = hex(&route_signing_preimage(&statement()).unwrap());
    // chain ‖ account LE8 ‖ label ‖ publisher ‖ revision ‖ route-present ‖
    // policy (audience, methods, caps, flags) ‖ target tag. The request cap is
    // tagged (`01` = a cap follows, `00` = this route declares none), so an
    // uncapped route and a capped one can never sign the same bytes.
    assert_eq!(
        encoded,
        "0400000000000000746573740700000000000000010300000000000000617069200000000000000003030303030303030303030303030303030303030303030303030303030303030700000000000000010203000000000000000102030100040000000000000010000000000000000102"
    );
}

#[test]
fn content_route_preimage_binds_only_the_manifest_hash() {
    let mut statement = statement();
    statement.route = Some(RouteDefinition {
        target: RouteTarget::DuckFs {
            manifest_sha256: "b".repeat(64),
        },
        policy: RoutePolicy {
            audience: RouteAudience::Network,
            methods: vec![RouteMethod::Get, RouteMethod::Head],
            max_request_bytes: Some(0),
            max_response_bytes: 4096,
            allow_authorization: false,
            allow_upgrade: false,
        },
    });
    let encoded = hex(&route_signing_preimage(&statement).unwrap());
    assert_eq!(
        encoded,
        "04000000000000007465737407000000000000000103000000000000006170692000000000000000030303030303030303030303030303030303030303030303030303030303030307000000000000000102020000000000000001020100000000000000000010000000000000000001bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
}

#[test]
fn route_json_rejects_unknown_fields() {
    let mut value = serde_json::to_value(statement()).unwrap();
    value["ambient_authority"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RouteStatement>(value).is_err());
}

#[test]
fn management_replies_keep_the_small_external_json_shape() {
    assert_eq!(
        serde_json::to_value(GatewayReply::Route(Box::new(None))).unwrap(),
        serde_json::json!({ "route": null })
    );
    let summary = RouteSummary {
        name: RouteName::named("api"),
        publisher_node: vec![3; 32],
        revision: 7,
        target: "loopback_http".into(),
    };
    assert_eq!(
        serde_json::to_value(GatewayReply::Routes(vec![summary])).unwrap(),
        serde_json::json!({
            "routes": [{
                "name": { "label": "api" },
                "publisher_node": vec![3; 32],
                "revision": 7,
                "target": "loopback_http"
            }]
        })
    );
}

/// There is no platform ceiling on a request body, and this is the test that
/// says so: a route may decline to name one at all, and one that names an
/// enormous one is not second-guessed. A cap is a publisher's statement about
/// its own lane — a git push carries a whole repository's history, and the
/// route that serves it declares `None`.
#[test]
fn a_route_may_decline_to_cap_its_request_body_at_all() {
    let policy = |max_request_bytes| RoutePolicy {
        audience: RouteAudience::Network,
        methods: vec![RouteMethod::Get, RouteMethod::Head, RouteMethod::Post],
        max_request_bytes,
        max_response_bytes: 4096,
        allow_authorization: false,
        allow_upgrade: false,
    };
    assert!(validate_policy(&policy(None)).is_ok());
    assert!(validate_policy(&policy(Some(16 * 1024 * 1024))).is_ok());
    assert!(validate_policy(&policy(Some(u64::MAX))).is_ok());
    // the one refusal left: a route that takes POST cannot also say it takes
    // no body, which would be a policy that contradicts itself.
    assert!(validate_policy(&policy(Some(0))).is_err());
}
