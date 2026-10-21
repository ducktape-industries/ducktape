use abi::role::validators::{Query, QueryPolicy, Reply};

#[test]
fn the_validators_role_asks_and_answers_at_valsets_own_tags() {
    assert_eq!(abi::encode(&Query::Validators), [0]);
    assert_eq!(abi::encode(&Query::Members), [1]);
    assert_eq!(abi::encode(&Query::QueryPolicy), [6]);
    assert_eq!(abi::encode(&Reply::Validators(Vec::new())), [0, 0, 0, 0, 0]);
    assert_eq!(abi::encode(&Reply::Members(Vec::new())), [1, 0, 0, 0, 0]);
    assert_eq!(
        abi::encode(&Reply::QueryPolicy(QueryPolicy::Signed)),
        [6, 0]
    );
    assert_eq!(abi::encode(&Reply::QueryPolicy(QueryPolicy::Open)), [6, 1]);
    assert_eq!(abi::decode::<Query>(&[6]).unwrap(), Query::QueryPolicy);
    assert_eq!(
        abi::decode::<Reply>(&[6, 1]).unwrap(),
        Reply::QueryPolicy(QueryPolicy::Open)
    );
}
