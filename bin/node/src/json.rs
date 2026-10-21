//! A program spoken to in JSON: the interface its live code's `ducktape.schema` section carries and
//! what its `ducktape.tools` section tells, read through programs' borsh-json as harness reads them
//! and kept in the workspace by code id, and an AddKey consent signed over identity's own admission.
use std::path::{Path, PathBuf};

use abi::{BlobId, Outcome, Receipt, Scheme, role};
use borsh_json::Interface;
use borsh_json::tools::Described;
use commonware_cryptography::{Signer as _, ed25519};
use host::Layer;
use node::{Frame, Preconfirm};
use noded::wire::{EgressPlan, Reached, Status, Submitted};
use noded::{Client, Workspace};
use serde_json::{Map, Value, json};

use crate::{Sentence as _, blob_text};

const SCHEMAS: &str = "schemas";
const TOOLS: &str = "tools";
const UNTOLD: &[u8] = b"{}";

pub async fn interface(
    workspace: &Workspace,
    client: &Client,
    program: &str,
) -> Result<Interface, String> {
    let programs = client.programs().await.sentence()?;
    let code = programs
        .get(program)
        .ok_or_else(|| format!("no program named {program} runs on this network"))?;
    let folder = schemas(workspace.dir());
    if let Some(interface) = kept(&folder, code).and_then(|kept| Interface::read(&kept).ok()) {
        return Ok(interface);
    }
    let section = fetched(client, program, code).await?;
    let interface = Interface::read(&section).map_err(|unreadable| {
        format!(
            "program {program}'s {} section {unreadable}",
            borsh_json::SECTION
        )
    })?;
    let _ = keep(&folder, code, &section);
    Ok(interface)
}

async fn fetched(client: &Client, program: &str, code: &BlobId) -> Result<Vec<u8>, String> {
    let framed = client
        .blob(*code)
        .await
        .sentence()?
        .ok_or_else(|| absent(program, code))?;
    let (_, module) = blob::parse(&framed).ok_or_else(|| absent(program, code))?;
    match section(module, borsh_json::SECTION).sentence()? {
        Some(section) => Ok(section.to_vec()),
        None => Err(format!(
            "program {program}'s code carries no {} section, so it has no schema",
            borsh_json::SECTION
        )),
    }
}

fn schemas(dir: &Path) -> PathBuf {
    dir.join(SCHEMAS)
}

fn schema_name(code: &BlobId) -> String {
    blob_text(code).replace(':', "-")
}

fn kept(folder: &Path, code: &BlobId) -> Option<Vec<u8>> {
    std::fs::read(folder.join(schema_name(code))).ok()
}

fn keep(folder: &Path, code: &BlobId, section: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let name = schema_name(code);
    let fresh = folder.join(format!("{name}.{}", std::process::id()));
    std::fs::write(&fresh, section)?;
    std::fs::rename(fresh, folder.join(name))
}

fn absent(program: &str, code: &BlobId) -> String {
    format!(
        "the node holds no code {} for program {program}",
        blob_text(code)
    )
}

fn section<'a>(
    module: &'a [u8],
    name: &str,
) -> Result<Option<&'a [u8]>, wasmparser::BinaryReaderError> {
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        if let wasmparser::Payload::CustomSection(custom) = payload?
            && custom.name() == name
        {
            return Ok(Some(custom.data()));
        }
    }
    Ok(None)
}

pub struct Code {
    pub interface: Result<Interface, String>,
    pub told: Result<Described, String>,
}

struct Sections {
    schema: Vec<u8>,
    tools: Vec<u8>,
}

impl Sections {
    fn read(&self, program: &str) -> Code {
        let interface = Interface::read(&self.schema).map_err(|unreadable| {
            format!(
                "program {program}'s {} section {unreadable}",
                borsh_json::SECTION
            )
        });
        let told = borsh_json::tools::read(&self.tools)
            .map_err(|unreadable| format!("program {program}'s code: {unreadable}"));
        Code { interface, told }
    }
}

pub async fn codes(
    workspace: &Workspace,
    client: &Client,
    programs: Vec<(String, BlobId)>,
) -> Result<Vec<(String, Result<Code, String>)>, String> {
    let schemas = schemas(workspace.dir());
    let tools = workspace.dir().join(TOOLS);
    let kept: Vec<Option<Code>> = programs
        .iter()
        .map(|(program, code)| {
            let sections = Sections {
                schema: kept(&schemas, code)?,
                tools: kept(&tools, code)?,
            };
            let read = sections.read(program);
            (read.interface.is_ok() && read.told.is_ok()).then_some(read)
        })
        .collect();
    let wanted: Vec<BlobId> = programs
        .iter()
        .zip(&kept)
        .filter(|(_, kept)| kept.is_none())
        .map(|((_, code), _)| *code)
        .collect();
    let fetched = match wanted.is_empty() {
        true => Vec::new(),
        false => client.blobs(&wanted).await.sentence()?,
    };
    let mut fetched = fetched.into_iter();
    let mut read = Vec::with_capacity(programs.len());
    for ((program, code), kept) in programs.into_iter().zip(kept) {
        let told = match kept {
            Some(code) => Ok(code),
            None => sections_in(&program, &code, fetched.next().flatten())
                .inspect(|sections| {
                    let _ = keep(&schemas, &code, &sections.schema);
                    let _ = keep(&tools, &code, &sections.tools);
                })
                .map(|sections| sections.read(&program)),
        };
        read.push((program, told));
    }
    Ok(read)
}

fn sections_in(program: &str, code: &BlobId, framed: Option<Vec<u8>>) -> Result<Sections, String> {
    let framed = framed.ok_or_else(|| absent(program, code))?;
    let (_, module) = blob::parse(&framed).ok_or_else(|| absent(program, code))?;
    let schema = section(module, borsh_json::SECTION)
        .sentence()?
        .ok_or_else(|| {
            format!(
                "program {program}'s code carries no {} section, so it has no schema",
                borsh_json::SECTION
            )
        })?;
    let tools = section(module, programs_sdk::tools::SECTION)
        .sentence()?
        .unwrap_or(UNTOLD);
    Ok(Sections {
        schema: schema.to_vec(),
        tools: tools.to_vec(),
    })
}

pub fn parse(text: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|fault| format!("the {what} is no JSON: {fault}"))
}

pub fn print(value: &Value) {
    println!("{value}");
}

pub async fn ask(
    client: &Client,
    key: &ed25519::PrivateKey,
    status: &Status,
    program: &str,
    request: Vec<u8>,
    layer: Layer,
) -> Result<Vec<u8>, String> {
    let frame = Frame::sign_query_as(
        crate::CLIENT,
        key,
        status.network.as_bytes(),
        status.anchor(),
        program,
        request,
    );
    client.query(layer, frame.encode()).await.sentence()
}

pub fn reply(interface: &Interface, program: &str, answer: &[u8]) -> Result<Value, String> {
    interface.decode_reply(answer).map_err(|fault| {
        format!(
            "the answer does not read as program {program}'s reply: {fault}; its bytes: {}",
            hex::encode(answer)
        )
    })
}

pub fn egress(plan: &EgressPlan) -> Value {
    let reach: Map<String, Value> = plan
        .reach
        .iter()
        .map(|(name, reached)| (name.clone(), json!(reached_text(reached))))
        .collect();
    json!({
        "tally": {
            "served": plan.served,
            "added": plan.added,
            "removed": plan.removed,
            "changed": plan.changed,
        },
        "reach": reach,
    })
}

fn reached_text(reached: &Reached) -> String {
    match reached {
        Reached::Answered(status) => format!("answered {status}"),
        Reached::Failed(why) => format!("failed: {why}"),
    }
}

pub struct Sent<'a> {
    pub interface: &'a Interface,
    pub program: &'a str,
    pub payload: &'a [u8],
}

impl Sent<'_> {
    pub async fn land(&self, client: &Client, frame: &Frame) -> Result<Value, String> {
        let (height, receipt) = match client.land(frame).await {
            Ok(landed) => landed,
            Err(noded::Error::Unlanded) => {
                return Err(format!(
                    "unlanded: no block carried frame {} before its anchor aged out",
                    hex::encode(frame.body.submission().id())
                ));
            }
            Err(error) => return Err(error.to_string()),
        };
        self.told(receipt, "landed", json!(height))
            .map_err(|why| format!("landed {height} {why}"))
    }

    pub async fn submit(
        &self,
        client: &Client,
        frame: &Frame,
        preconfirm: Preconfirm,
    ) -> Result<Value, String> {
        match client.submit(frame.encode(), preconfirm).await.sentence()? {
            Submitted::Pooled => {
                let id = hex::encode(frame.body.submission().id());
                Ok(json!({ "pooled": id }))
            }
            Submitted::Preconfirmed(receipt) => self
                .told(receipt, "preconfirmed", json!(true))
                .map_err(|why| format!("preconfirmed {why}")),
        }
    }

    fn told(&self, receipt: Receipt, at: &str, mark: Value) -> Result<Value, String> {
        let output = match receipt.outcome {
            Outcome::Applied { output } => output,
            Outcome::Rejected(refusal) => {
                return Err(format!("rejected {}: {}", refusal.reason, refusal.sentence));
            }
        };
        let mut told = Map::new();
        told.insert(at.to_owned(), mark);
        let returned = self.interface.returned(self.program, self.payload, &output);
        if let Value::Object(returned) = returned {
            told.extend(returned);
        }
        if !receipt.events.is_empty() {
            let events: Vec<String> = receipt.events.iter().map(hex::encode).collect();
            told.insert("events".to_owned(), json!(events));
        }
        Ok(Value::Object(told))
    }
}

pub struct Consenting {
    pub account: u64,
    pub key: Option<Vec<u8>>,
    pub scheme: Scheme,
    pub network: Option<String>,
    pub expires: Option<u64>,
}

pub async fn consent(
    client: &Client,
    signer: &ed25519::PrivateKey,
    asked: Consenting,
) -> Result<Value, String> {
    let status = client.status().await.sentence()?;
    if let Some(network) = &asked.network
        && *network != status.network
    {
        return Err(format!(
            "the node runs network {}, not {network}",
            status.network
        ));
    }
    let key = asked
        .key
        .unwrap_or_else(|| signer.public_key().as_ref().to_vec());
    let program = status
        .roles
        .bound(role::IDENTITY)
        .map_err(|refusal| refusal.sentence)?
        .to_owned();
    let query = identity::Query::Generation { key: key.clone() };
    let request = abi::encode(&query);
    let answer = ask(
        client,
        signer,
        &status,
        &program,
        request,
        Layer::Preconfirmed,
    )
    .await?;
    let generation = match abi::decode(&answer).map_err(|refusal| refusal.sentence)? {
        identity::Reply::Generation(generation) => generation,
        _ => return Err(format!("program {program} answered no generation")),
    };
    let expires_at = asked.expires.unwrap_or(u64::MAX);
    let admission = identity::Admission {
        network: status.network.into_bytes(),
        scheme: asked.scheme,
        key,
        generation,
        account: asked.account,
        expires_at,
    };
    Ok(signed(signer, &admission))
}

pub fn signed(signer: &ed25519::PrivateKey, admission: &identity::Admission) -> Value {
    let proof = signer.sign(identity::CONSENT_NAMESPACE, &admission.preimage());
    json!({
        "key": hex::encode(signer.public_key().as_ref()),
        "account": admission.account,
        "expires_at": admission.expires_at,
        "proof": hex::encode(proof.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use abi::{Outcome, Receipt, Scheme};
    use borsh_json::Interface;
    use commonware_cryptography::{Signer as _, ed25519};
    use serde_json::json;

    use super::{Sent, keep, kept, schemas, section, signed};

    fn module(customs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"\0asm\x01\0\0\0".to_vec();
        for (name, payload) in customs {
            let mut body = vec![name.len() as u8];
            body.extend(name.as_bytes());
            body.extend(*payload);
            out.push(0);
            out.push(body.len() as u8);
            out.extend(body);
        }
        out
    }

    fn ops() -> Interface {
        let schema = borsh::schema::BorshSchemaContainer::for_type::<Roots>();
        Interface::read(&abi::encode(&schema)).unwrap()
    }

    #[derive(borsh::BorshSchema)]
    #[allow(dead_code)]
    struct Roots {
        op: Op,
        query: u8,
        reply: u64,
        returns: Returns,
    }

    #[derive(borsh::BorshSchema, borsh::BorshSerialize)]
    #[allow(dead_code)]
    enum Op {
        Count { by: u64 },
        Reset,
    }

    #[derive(borsh::BorshSchema)]
    #[allow(dead_code, non_snake_case)]
    struct Returns {
        Count: u64,
    }

    #[test]
    fn a_kept_schema_reads_back_under_its_own_code_id_alone() {
        let dir = tempfile::tempdir().unwrap();
        let folder = schemas(dir.path());
        let code = abi::BlobId::Sha256([1; 32]);
        let other = abi::BlobId::Sha256([2; 32]);
        assert_eq!(kept(&folder, &code), None);
        keep(&folder, &code, b"schema").unwrap();
        assert_eq!(kept(&folder, &code), Some(b"schema".to_vec()));
        assert_eq!(kept(&folder, &other), None);
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    }

    #[test]
    fn each_section_is_found_by_its_name_among_a_codes_custom_sections() {
        let code = module(&[
            ("ducktape.view", b"view"),
            (borsh_json::SECTION, b"schema"),
            (programs_sdk::tools::SECTION, b"{}"),
        ]);
        assert_eq!(
            section(&code, borsh_json::SECTION).unwrap(),
            Some(&b"schema"[..])
        );
        assert_eq!(
            section(&code, programs_sdk::tools::SECTION).unwrap(),
            Some(&b"{}"[..])
        );
        let viewed = module(&[("ducktape.view", b"view")]);
        assert_eq!(section(&viewed, borsh_json::SECTION).unwrap(), None);
        assert!(section(b"not a module", borsh_json::SECTION).is_err());
    }

    #[test]
    fn an_applied_op_reads_as_what_it_returns_and_a_rejected_one_is_an_error() {
        let interface = ops();
        let payload = abi::encode(&Op::Count { by: 2 });
        let sent = Sent {
            interface: &interface,
            program: "counter",
            payload: &payload,
        };
        let applied = Receipt {
            program: "counter".into(),
            outcome: Outcome::Applied {
                output: abi::encode(&5u64),
            },
            events: vec![vec![1, 2]],
            nested: Vec::new(),
        };
        assert_eq!(
            sent.told(applied, "landed", json!(9)).unwrap(),
            json!({ "landed": 9, "ok": true, "returned": 5, "events": ["0102"] })
        );
        let rejected = Receipt {
            program: "counter".into(),
            outcome: Outcome::Rejected(abi::Refusal::new("unauthorized", "not yours")),
            events: Vec::new(),
            nested: Vec::new(),
        };
        let refused = sent.told(rejected, "landed", json!(9)).unwrap_err();
        assert_eq!(refused, "rejected unauthorized: not yours");
    }

    #[test]
    fn a_consent_verifies_over_identitys_admission_as_the_network_checks_it() {
        let signer = ed25519::PrivateKey::from_seed(7);
        let admission = identity::Admission {
            network: b"sealpond".to_vec(),
            scheme: Scheme::Ed25519,
            key: signer.public_key().as_ref().to_vec(),
            generation: 0,
            account: 18,
            expires_at: u64::MAX,
        };
        let consent = signed(&signer, &admission);
        let key = hex::decode(consent["key"].as_str().unwrap()).unwrap();
        let proof = hex::decode(consent["proof"].as_str().unwrap()).unwrap();
        assert_eq!(key, admission.key);
        assert_eq!(consent["account"], json!(18));
        assert_eq!(consent["expires_at"], json!(u64::MAX));
        let verifies = |preimage: &[u8]| {
            keyscheme::KeyScheme::Ed25519.verify(
                &key,
                identity::CONSENT_NAMESPACE,
                preimage,
                &proof,
            )
        };
        assert!(verifies(&admission.preimage()));
        let elsewhere = identity::Admission {
            network: b"duckpond".to_vec(),
            ..admission
        };
        assert!(!verifies(&elsewhere.preimage()));
    }
}
