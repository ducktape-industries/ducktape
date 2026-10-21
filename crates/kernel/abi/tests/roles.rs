use abi::Origin;
use abi::role::{access, egress, identity, ownership, programs};

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

#[test]
fn the_egress_role_takes_unregister_after_expire_at_tag_8() {
    assert_eq!(tag(&egress::Op::Expire { id: 1 }), 7);
    let unregister = egress::Op::Unregister {
        service: "s".into(),
    };
    assert_eq!(abi::encode(&unregister), [8, 1, 0, 0, 0, b's']);
}

#[test]
fn renames_ask_at_tag_12_answer_at_tag_9_and_rename_program_takes_identity_tag_12() {
    assert_eq!(
        abi::encode(&programs::Query::Renames(1)),
        [12, 1, 0, 0, 0, 0, 0, 0, 0]
    );
    let renames = programs::Reply::Renames(vec![("a".to_owned(), "o/a".to_owned())]);
    assert_eq!(
        abi::encode(&renames),
        [
            9, 1, 0, 0, 0, 1, 0, 0, 0, b'a', 3, 0, 0, 0, b'o', b'/', b'a'
        ]
    );
    let rename = identity::Op::RenameProgram {
        from: "a".into(),
        to: "o/a".into(),
    };
    assert_eq!(
        abi::encode(&rename),
        [12, 1, 0, 0, 0, b'a', 3, 0, 0, 0, b'o', b'/', b'a']
    );
}

#[test]
fn reads_asks_answers_and_readies_at_their_tags() {
    use abi::role::reads::{self, Entry, Op, State};
    let read = Op::Read {
        target: "p".into(),
        query: vec![7],
    };
    assert_eq!(abi::encode(&read), [0, 1, 0, 0, 0, b'p', 1, 0, 0, 0, 7]);
    let entry = Entry {
        id: abi::BlobId::Sha256([0; 32]),
        len: 1,
        kind: "k".into(),
    };
    let answer = Op::Answer {
        id: 2,
        at: 3,
        outcome: Ok(entry.clone()),
    };
    assert_eq!(tag(&answer), 1);
    assert_eq!(
        abi::encode(&Op::Ready { id: 2 }),
        [2, 2, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(tag(&State::Asked), 0);
    let read = reads::Read { entry, at: 3 };
    assert_eq!(tag(&State::Putting { bundle: 0, read }), 1);
    let members = [vec![1], vec![2]];
    assert_eq!(reads::assigned(5, &members), Some([2].as_slice()));
    assert_eq!(reads::assigned(4, &members), Some([1].as_slice()));
    assert_eq!(reads::assigned(4, &[]), None);
}

#[test]
fn a_program_id_is_one_segment_or_an_owner_and_a_name() {
    assert!(abi::program_id_shaped("chat"));
    assert!(abi::program_id_shaped("ducktape-industries/chat"));
    for refused in ["", "/", "a/", "/a", "a/b/c", "a//b", "a@5", "o/a@5"] {
        assert!(!abi::program_id_shaped(refused), "{refused}");
    }
}
