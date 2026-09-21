use duckdns::{
    DuckDnsName, RESERVED_ROOT_LABELS, parse_hostname, validate_handle, validate_handle_shape,
};

#[test]
fn account_name_round_trips_canonically() {
    let name = parse_hostname("orthory.duck").unwrap();
    assert_eq!(
        name,
        DuckDnsName {
            handle: "orthory".into(),
        }
    );
    assert_eq!(name.hostname(), "orthory.duck");
}

#[test]
fn lookup_normalizes_dns_case_and_one_trailing_dot() {
    let name = parse_hostname("Orthory.DUCK.").unwrap();
    assert_eq!(
        name,
        DuckDnsName {
            handle: "orthory".into(),
        }
    );
    assert_eq!(name.hostname(), "orthory.duck");
}

#[test]
fn strict_handles_and_reserved_roots_are_enforced() {
    for valid in ["a", "abc-123", &"z".repeat(63)] {
        validate_handle(valid).unwrap();
    }
    for invalid in ["", "-a", "a-", "A", "a_b", "a.b", "döcs"] {
        assert!(validate_handle(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(validate_handle(&"z".repeat(64)).is_err());
    assert!(RESERVED_ROOT_LABELS.contains(&"net"));
    assert!(validate_handle("net").unwrap_err().contains("reserved"));
}

/// SHAPE vs POLICY. The shape rule is frozen: it is what state validation and
/// snapshot DECODING enforce, and it must accept every handle any past binary
/// committed. The reserved set is policy: it grows, it is read only at
/// ADMISSION, and a label it gains must stay decodable — otherwise reserving one
/// retroactively bricks state sync and checkpoint restore on every network that
/// already carries it. Keep the two rules from collapsing back into one.
#[test]
fn reserved_labels_are_admission_policy_and_never_a_shape_rule() {
    for label in RESERVED_ROOT_LABELS {
        validate_handle_shape(label)
            .unwrap_or_else(|e| panic!("a reserved label is still a well-SHAPED handle: {e}"));
        assert!(validate_handle(label).unwrap_err().contains("reserved"));
    }
    // shape still rejects what a handle can never be, reserved or not.
    for invalid in ["", "-a", "a-", "A", "a_b", "a.b", "döcs"] {
        assert!(validate_handle_shape(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn non_account_names_reject() {
    for hostname in [
        "orthory.example",
        "net.duck",
        "huddle.orthory.duck",
        "orthory.duck..",
        " orthory.duck",
        "döcs.duck",
    ] {
        assert!(parse_hostname(hostname).is_err(), "accepted {hostname:?}");
    }
}
