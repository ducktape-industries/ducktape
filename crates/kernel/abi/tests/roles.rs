use abi::Origin;
use abi::role::{access, identity, ownership};

fn tag(value: &impl borsh::BorshSerialize) -> u8 {
    abi::encode(value)[0]
}

#[test]
fn the_identity_role_asks_and_answers_at_its_tags() {
    let program = || "p".to_owned();
    assert_eq!(
        abi::encode(&identity::Op::RegisterProgram { program: program() }),
        [0, 1, 0, 0, 0, b'p']
    );
    assert_eq!(
        abi::encode(&identity::Query::Account(Vec::new())),
        [0, 0, 0, 0, 0]
    );
    assert_eq!(
        abi::encode(&identity::Query::OfProgram(program())),
        [1, 1, 0, 0, 0, b'p']
    );
    assert_eq!(tag(&identity::Query::Profile(7)), 2);
    let profiles = identity::Query::Profiles {
        after: None,
        limit: 1,
    };
    assert_eq!(abi::encode(&profiles), [3, 0, 1, 0, 0, 0]);
    let runs = identity::Query::Runs {
        program: program(),
        account: 7,
    };
    assert_eq!(tag(&runs), 4);
    assert_eq!(abi::encode(&identity::Reply::Account(None)), [0, 0]);
    assert_eq!(abi::encode(&identity::Reply::Profile(None)), [1, 0]);
    let page = identity::Reply::Profiles {
        profiles: Vec::new(),
        next: None,
    };
    assert_eq!(abi::encode(&page), [2, 0, 0, 0, 0, 0]);
}

#[test]
fn the_access_role_asks_and_answers_at_its_tags() {
    let check = access::Op::Check {
        program: "p".into(),
        actor: Vec::new(),
        needs: Vec::new(),
    };
    assert_eq!(
        abi::encode(&check),
        [0, 1, 0, 0, 0, b'p', 0, 0, 0, 0, 0, 0, 0, 0]
    );
    let at = |tag: u8| [tag, 1, 0, 0, 0, b'p', 1, 0, 0, 0, b'k'];
    let (program, key) = (|| "p".to_owned(), || "k".to_owned());
    let list = access::Query::List {
        program: program(),
        key: key(),
    };
    let resource = access::Query::Resource {
        program: program(),
        key: key(),
    };
    let writers = access::Query::Writers {
        program: program(),
        key: key(),
    };
    assert_eq!(abi::encode(&list), at(0));
    assert_eq!(abi::encode(&resource), at(1));
    assert_eq!(abi::encode(&writers), at(2));
    assert_eq!(abi::encode(&access::Reply::List(None)), [0, 0]);
    assert_eq!(
        tag(&access::Reply::Resource(access::Resource::default())),
        1
    );
    assert_eq!(tag(&access::Reply::Writers(access::Writers::Anyone)), 2);
}

#[test]
fn the_ownership_role_takes_its_guard_at_tag_0() {
    let guard = ownership::Guard {
        program: "p".into(),
        origin: Origin::System,
        sender: None,
        writes: Vec::new(),
    };
    assert_eq!(tag(&ownership::Op::Guard(guard)), 0);
}
