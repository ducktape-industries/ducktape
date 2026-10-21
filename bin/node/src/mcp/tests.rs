use std::cell::RefCell;
use std::collections::BTreeMap;

use abi::role::identity::{Category, Kind, Profile, Standing};
use abi::{BlobId, Outcome, Refusal};
use borsh_json::Interface;
use borsh_json::discover;
use borsh_json::tools::{Described, Told};
use serde_json::{Value, json};

use super::{Landed, Network, serve};
use crate::json::Code;

#[derive(borsh::BorshSchema)]
#[allow(dead_code)]
struct Roots {
    op: Op,
    query: Ask,
    reply: Answer,
    returns: Returns,
}

#[derive(borsh::BorshSchema, borsh::BorshSerialize)]
#[allow(dead_code)]
enum Op {
    Count { by: u64 },
    Reset,
}

#[derive(borsh::BorshSchema, borsh::BorshSerialize)]
#[allow(dead_code)]
enum Ask {
    Total,
}

#[derive(borsh::BorshSchema, borsh::BorshSerialize)]
#[allow(dead_code)]
enum Answer {
    Total(u64),
}

#[derive(borsh::BorshSchema)]
#[allow(dead_code, non_snake_case)]
struct Returns {
    Count: u64,
}

const WHOM: &str = "You act as account 7.";
const COUNTER: &str = "counts what it is told";
const COUNT: &str = "add by to the count";

fn code() -> Code {
    let schema = borsh::schema::BorshSchemaContainer::for_type::<Roots>();
    let told = Told {
        description: COUNT.into(),
        example: Some(json!({ "count": { "by": 1 } })),
        returns: None,
    };
    let described = Described {
        about: COUNTER.into(),
        ops: BTreeMap::from([("Count".to_owned(), told)]),
        queries: BTreeMap::new(),
    };
    Code {
        interface: Interface::read(&abi::encode(&schema)).map_err(|fault| fault.to_string()),
        told: Ok(described),
    }
}

fn read_back() -> files::Contents {
    files::Contents {
        snapshot: BlobId::Sha256([3; 32]),
        size: 5,
        offset: 0,
        bytes: b"hello".to_vec(),
        eof: true,
    }
}

#[derive(Default)]
struct Fake {
    sent: RefCell<Vec<(String, Vec<u8>)>>,
    put: RefCell<Vec<(String, Vec<Vec<u8>>)>>,
}

impl Fake {
    fn answered(&self, query: files::Query) -> files::Reply {
        match query {
            files::Query::Head => files::Reply::Head(None),
            files::Query::Stat { at, .. } => files::Reply::Stat {
                snapshot: at,
                entry: None,
            },
            files::Query::Read { .. } => files::Reply::Contents(read_back()),
            other => panic!("files was asked {other:?}"),
        }
    }
}

impl Network for Fake {
    async fn whom(&self) -> String {
        WHOM.into()
    }

    async fn running(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<(String, Result<Code, String>)>, String> {
        let all = vec![
            ("counter".to_owned(), Ok(code())),
            (
                "broken".to_owned(),
                Err("its code does not read".to_owned()),
            ),
        ];
        Ok(all
            .into_iter()
            .filter(|(program, _)| only.is_none_or(|only| program.as_str() == only))
            .collect())
    }

    async fn query(&self, program: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        match program {
            "counter" => Ok(abi::encode(&Answer::Total(7))),
            "files" => Ok(abi::encode(&self.answered(abi::decode(&request).unwrap()))),
            other => panic!("{other} was asked"),
        }
    }

    async fn send(&self, program: &str, payload: Vec<u8>) -> Result<Landed, String> {
        self.sent
            .borrow_mut()
            .push((program.to_owned(), payload.clone()));
        let refused = payload == abi::encode(&Op::Reset);
        let outcome = match (program, refused) {
            (_, true) => Outcome::Rejected(Refusal::new("unauthorized", "not yours")),
            ("files", false) => Outcome::Applied {
                output: abi::encode(&BlobId::Sha256([9; 32])),
            },
            (_, false) => Outcome::Applied {
                output: abi::encode(&5u64),
            },
        };
        Ok(Landed { height: 4, outcome })
    }

    async fn egress(&self) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn put(&self, kind: &str, bodies: Vec<Vec<u8>>) -> Result<Vec<BlobId>, String> {
        let ids = (0..bodies.len())
            .map(|at| BlobId::Sha256([at as u8 + 1; 32]))
            .collect();
        self.put.borrow_mut().push((kind.to_owned(), bodies));
        Ok(ids)
    }
}

fn session(fake: &Fake, messages: &[Value]) -> Vec<Value> {
    let input: String = messages
        .iter()
        .map(|message| format!("{message}\n"))
        .collect();
    exchanged(fake, &input)
}

fn exchanged(fake: &Fake, input: &str) -> Vec<Value> {
    let mut output = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(serve(fake, input.as_bytes(), &mut output))
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn answer(answers: &[Value], id: u64) -> &Value {
    answers
        .iter()
        .find(|answer| answer["id"] == json!(id))
        .unwrap_or_else(|| panic!("no answer to {id} in {answers:?}"))
}

fn calling(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

fn told(answers: &[Value], id: u64) -> Value {
    let result = &answer(answers, id)["result"];
    assert_eq!(result.get("isError"), None, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn refused(answers: &[Value], id: u64) -> String {
    let result = &answer(answers, id)["result"];
    assert_eq!(result["isError"], json!(true), "{result}");
    result["content"][0]["text"].as_str().unwrap().to_owned()
}

#[test]
fn a_session_initializes_lists_its_tools_and_answers_each_network_tool() {
    let fake = Fake::default();
    let answers = session(
        &fake,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            calling(3, "network_schema", json!({})),
            calling(
                4,
                "network_schema",
                json!({ "program": "counter", "op": "count" }),
            ),
            calling(
                5,
                "network_query",
                json!({ "program": "counter", "query": "total" }),
            ),
            calling(
                6,
                "network_call",
                json!({ "program": "counter", "op": { "count": { "by": 2 } } }),
            ),
            json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }),
        ],
    );
    assert_eq!(
        answers.len(),
        7,
        "a notification takes no answer: {answers:?}"
    );

    let initialized = &answer(&answers, 1)["result"];
    assert_eq!(initialized["protocolVersion"], json!("2025-03-26"));
    assert_eq!(initialized["instructions"], json!(WHOM));
    assert_eq!(initialized["serverInfo"]["name"], json!("ducktape"));
    assert_eq!(
        initialized["capabilities"]["tools"],
        json!({ "listChanged": false })
    );

    let listed = answer(&answers, 2)["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = listed
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "network_schema",
            "network_query",
            "network_call",
            "files_read",
            "files_write"
        ]
    );
    assert_eq!(listed[2]["description"], json!(discover::CALL_DESCRIPTION));
    assert_eq!(listed[2]["inputSchema"], discover::call_input());
    assert_eq!(listed[0]["inputSchema"], discover::schema_input());

    assert_eq!(
        told(&answers, 3),
        json!({ "programs": [
            { "program": "counter", "about": COUNTER },
            { "program": "broken", "unread": "its code does not read" },
        ] })
    );
    let unfolded = told(&answers, 4);
    assert_eq!(unfolded["description"], json!(COUNT));
    assert_eq!(unfolded["example"], json!({ "count": { "by": 1 } }));
    assert_eq!(told(&answers, 5), json!({ "reply": { "total": 7 } }));
    assert_eq!(
        told(&answers, 6),
        json!({ "ok": true, "returned": 5, "landed": 4 })
    );
    assert_eq!(
        *fake.sent.borrow(),
        vec![("counter".to_owned(), abi::encode(&Op::Count { by: 2 }))]
    );
    assert_eq!(answer(&answers, 7)["result"], json!({}));
}

#[test]
fn a_refused_encode_or_a_rejected_op_is_a_tool_error_and_a_bad_message_a_protocol_one() {
    let fake = Fake::default();
    let answers = session(
        &fake,
        &[
            calling(
                1,
                "network_call",
                json!({ "program": "counter", "op": { "count": { "by": "two" } } }),
            ),
            calling(
                2,
                "network_call",
                json!({ "program": "counter", "op": "reset" }),
            ),
            calling(
                3,
                "network_schema",
                json!({ "program": "counter", "op": "shout" }),
            ),
            calling(4, "network_schema", json!({ "program": "nobody" })),
            calling(5, "chat_post", json!({})),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/list" }),
        ],
    );
    let encode = refused(&answers, 1);
    assert!(encode.starts_with("at op.count.by: "), "{encode}");
    assert_eq!(
        *fake.sent.borrow(),
        vec![("counter".to_owned(), abi::encode(&Op::Reset))],
        "an op that does not encode is never sent"
    );
    assert_eq!(refused(&answers, 2), "rejected unauthorized: not yours");
    let unknown = refused(&answers, 3);
    assert!(
        unknown.starts_with("program counter has no op shout; its ops are count, reset"),
        "{unknown}"
    );
    assert_eq!(
        refused(&answers, 4),
        "no program named nobody runs on this network"
    );
    let tool = &answer(&answers, 5)["error"];
    assert_eq!(tool["code"], json!(-32602));
    assert!(
        tool["message"]
            .as_str()
            .unwrap()
            .contains("there is no tool chat_post; the tools are network_schema"),
        "{tool}"
    );
    assert_eq!(answer(&answers, 6)["error"]["code"], json!(-32601));

    let broken = exchanged(
        &fake,
        "{not json\n\n{\"jsonrpc\": \"2.0\", \"id\": 8, \"result\": {}}\n",
    );
    assert_eq!(
        broken.len(),
        1,
        "a blank line and a response take no answer: {broken:?}"
    );
    assert_eq!(broken[0]["id"], Value::Null);
    assert_eq!(broken[0]["error"]["code"], json!(-32700));
}

#[test]
fn a_file_is_written_as_its_chunks_then_one_commit_and_read_back_as_text() {
    let fake = Fake::default();
    let big = "a".repeat(files::CHUNK_SIZE as usize + 1);
    let answers = session(
        &fake,
        &[
            calling(
                1,
                "files_write",
                json!({ "path": "/shared/a.md", "content": "hello", "message": "a note" }),
            ),
            calling(2, "files_read", json!({ "path": "/shared/a.md" })),
        ],
    );
    let snapshot = hex::encode([9; 32]);
    assert_eq!(
        told(&answers, 1),
        json!({ "snapshot": snapshot, "landed": 4 })
    );
    assert_eq!(
        *fake.put.borrow(),
        vec![("chunk".to_owned(), vec![b"hello".to_vec()])]
    );
    let commit = files::Op::Commit {
        base: None,
        message: "a note".into(),
        changes: vec![files::Change::Put {
            path: "/shared/a.md".into(),
            exec: false,
            meta: BTreeMap::new(),
            content: files::Content::Chunks {
                size: 5,
                chunks: vec![BlobId::Sha256([1; 32])],
            },
        }],
    };
    assert_eq!(
        *fake.sent.borrow(),
        vec![("files".to_owned(), abi::encode(&commit))]
    );
    assert_eq!(
        told(&answers, 2),
        json!({ "snapshot": hex::encode([3; 32]), "size": 5, "offset": 0, "eof": true, "text": "hello" })
    );

    let fake = Fake::default();
    let answers = session(
        &fake,
        &[
            calling(
                1,
                "files_write",
                json!({ "path": "/shared/big", "content": big }),
            ),
            calling(
                2,
                "files_write",
                json!({ "path": "/shared/empty", "content": "" }),
            ),
        ],
    );
    told(&answers, 1);
    told(&answers, 2);
    let put = fake.put.borrow();
    assert_eq!(put.len(), 1, "an empty file puts no chunk");
    let sizes: Vec<usize> = put[0].1.iter().map(Vec::len).collect();
    assert_eq!(sizes, [files::CHUNK_SIZE as usize, 1]);
}

fn profile(kind: Kind) -> Profile {
    Profile {
        number: 60,
        name: "scout".into(),
        kind,
    }
}

#[test]
fn only_an_agent_accounts_key_is_served_and_any_other_is_refused_with_how_to_make_one() {
    let key = "ab";
    let agent = Kind::Program {
        manager: 21,
        category: Category::Agent,
        standing: Standing::Active,
        script: None,
    };
    let (seated, manager) = super::agent(key, Some(profile(agent))).unwrap();
    assert_eq!((seated.number, manager), (60, 21));
    let person = super::agent(key, Some(profile(Kind::User))).unwrap_err();
    assert!(
        person.starts_with("the key ab acts as account 60 (scout), a person's own account. ducktape mcp signs only as an agent account"),
        "{person}"
    );
    assert!(person.contains("create_agent"), "{person}");
    let system = super::agent(key, Some(profile(Kind::System("chat".into())))).unwrap_err();
    assert!(system.contains("program chat's own account"), "{system}");
    let none = super::agent(key, None).unwrap_err();
    assert!(
        none.starts_with("the key ab acts as no account. "),
        "{none}"
    );
    assert!(none.contains("add_key"), "{none}");
}
