use super::*;

const DUCKPOND: &str = "725bcad1000000000000000000000000000000000000000000000000000000ff";

fn linked(name: &str, network: &str, base: &str) -> String {
    format!("[crosspond.{name}]\nnetwork = \"{network}\"\nbase = \"{base}\"\n")
}

fn network(text: &str) -> NetworkId {
    NetworkId::try_from(hex::decode(text).unwrap()).unwrap()
}

#[test]
fn a_crosspond_entry_names_the_network_and_base_and_the_node_serves_crosspond() {
    let text = linked("duckpond", DUCKPOND, "http://10.43.199.205:8844/");
    let config = Config::parse(&text, env, unread).unwrap();
    let peer = Peer {
        network: network(DUCKPOND),
        base: Url::parse("http://10.43.199.205:8844").unwrap(),
    };
    assert_eq!(config.crosspond["duckpond"], peer);
    assert_eq!(config.served(), [CROSSPOND]);
    assert_eq!(config.peer(&network(DUCKPOND)), Some(("duckpond", &peer)));
    assert_eq!(config.peer(&[0; 32]), None);
    let none = Config::parse("relay = true\n", env, unread).unwrap();
    assert_eq!(none.served(), [RELAY]);
}

#[test]
fn a_network_that_is_no_digest_one_named_twice_or_an_unfit_base_is_refused() {
    let short = linked("duckpond", "725bcad1", "http://10.43.199.205:8844");
    let unhexed = linked("duckpond", &"zz".repeat(32), "http://10.43.199.205:8844");
    for text in [short, unhexed] {
        let refused = Config::parse(&text, env, unread).unwrap_err();
        assert!(matches!(refused, Refused::Network { .. }), "{refused:?}");
        assert_eq!(refused.reason(), reason::INVALID_INPUT);
    }
    let twice = [
        linked("duckpond", DUCKPOND, "http://10.43.199.205:8844"),
        linked("pond", DUCKPOND, "http://10.43.199.206:8844"),
    ]
    .concat();
    let refused = Config::parse(&twice, env, unread).unwrap_err();
    let Refused::Twice { first, second } = &refused else {
        panic!("not refused for the repeated network: {refused:?}");
    };
    assert_eq!((first.as_str(), second.as_str()), ("duckpond", "pond"));
    let unfit = linked("duckpond", DUCKPOND, "ftp://10.43.199.205");
    let refused = Config::parse(&unfit, env, unread).unwrap_err();
    let Refused::Unfit { service } = &refused else {
        panic!("not refused for its base: {refused:?}");
    };
    assert_eq!(service, "crosspond.duckpond");
    let stray = format!(
        "{}lent = true\n",
        linked("duckpond", DUCKPOND, "http://h:1")
    );
    let refused = Config::parse(&stray, env, unread).unwrap_err();
    assert!(matches!(refused, Refused::Unparsed(_)), "{refused:?}");
}
