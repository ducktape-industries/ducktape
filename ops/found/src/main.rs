// Encodes and decodes the identity, egress, harness, files, access and chat ops and queries that founding and seeding a network send; used by seed.sh.
use std::error::Error;
use std::io::{Read, Write};

use program::abi::Scheme;
use store::PageRequest;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn arg(args: &[String], at: usize) -> Result<&str> {
    args.get(at)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument {at}").into())
}

fn number(args: &[String], at: usize) -> Result<u64> {
    Ok(arg(args, at)?.parse()?)
}

fn stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn encode<T: borsh::BorshSerialize>(value: &T) -> Result<()> {
    std::io::stdout().write_all(&borsh::to_vec(value)?)?;
    Ok(())
}

fn text(path: &str) -> Result<String> {
    Ok(std::fs::read_to_string(path)?)
}

fn party(word: &str) -> program::Party {
    match word.parse::<u64>() {
        Ok(number) => program::Party::Account(number),
        Err(_) => program::Party::Program(word.to_owned()),
    }
}

fn list(word: &str) -> Result<Option<program::Ownership>> {
    match word {
        "none" => Ok(None),
        "owner" => Ok(Some(program::Ownership::Owner)),
        "anyone" => Ok(Some(program::Ownership::Anyone)),
        listed => {
            let Some(parties) = listed.strip_prefix("listed:") else {
                return Err(
                    format!("a list is none, owner, anyone or listed:a,b: {listed}").into(),
                );
            };
            Ok(Some(program::Ownership::Listed(
                parties
                    .split(',')
                    .filter(|w| !w.is_empty())
                    .map(party)
                    .collect(),
            )))
        }
    }
}

fn blob_id(hex_id: &str) -> Result<program::BlobId> {
    let id = hex_id.strip_prefix("sha256:").unwrap_or(hex_id);
    let bytes: [u8; 32] = hex::decode(id)?
        .try_into()
        .map_err(|_| "a blob id is 32 bytes")?;
    Ok(program::BlobId::Sha256(bytes))
}

fn consent(args: &[String]) -> Result<()> {
    use commonware_codec::Encode as _;
    use commonware_cryptography::Signer as _;
    let password = text(arg(args, 2)?)?;
    let key =
        keystore::userkey::open_user_key_at(std::path::Path::new(arg(args, 1)?), password.trim())?;
    let public = key.public_key().encode().to_vec();
    let account = number(args, 3)?;
    let expires_at = number(args, 6)?;
    let admission = identity::Admission {
        network: arg(args, 4)?.as_bytes().to_vec(),
        scheme: Scheme::Ed25519,
        key: public.clone(),
        generation: number(args, 5)?,
        account,
        expires_at,
    };
    let proof = key
        .sign(identity::CONSENT_NAMESPACE, &admission.preimage())
        .encode()
        .to_vec();
    encode(&identity::Op::AddKey {
        scheme: Scheme::Ed25519,
        label: Some(arg(args, 7)?.to_owned()),
        consent: identity::Consent {
            key: public,
            account,
            expires_at,
            proof,
        },
    })
}

fn page(next: Option<&String>) -> Result<PageRequest> {
    let after = next.map(hex::decode).transpose()?;
    Ok(PageRequest { after, limit: None })
}

fn files_commit(args: &[String]) -> Result<()> {
    let base = match arg(args, 1)? {
        "none" => None,
        id => Some(blob_id(id)?),
    };
    let mut changes = Vec::new();
    let mut at = 3;
    while at < args.len() {
        if arg(args, at)? == "--mkdir" {
            changes.push(files::Change::Mkdir {
                path: arg(args, at + 1)?.to_owned(),
            });
            at += 2;
            continue;
        }
        let path = arg(args, at)?.to_owned();
        let size = number(args, at + 1)?;
        let count = number(args, at + 2)? as usize;
        let chunks = args[at + 3..at + 3 + count]
            .iter()
            .map(|id| blob_id(id))
            .collect::<Result<_>>()?;
        changes.push(files::Change::Put {
            path,
            exec: false,
            meta: Default::default(),
            content: files::Content::Chunks { size, chunks },
        });
        at += 3 + count;
    }
    encode(&files::Op::Commit {
        base,
        message: arg(args, 2)?.to_owned(),
        changes,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match arg(&args, 0)? {
        "create" => encode(&identity::Op::Create {
            name: arg(&args, 1)?.to_owned(),
            scheme: Scheme::Ed25519,
        }),
        "create-agent" => encode(&identity::Op::CreateAgent {
            name: arg(&args, 1)?.to_owned(),
        }),
        "set-script" => encode(&identity::Op::SetScript {
            account: number(&args, 1)?,
            script: Some(identity::Script {
                calls: harness::PROGRAM.to_owned(),
                config: arg(&args, 2)?.to_owned(),
            }),
        }),
        "register" => encode(&egress::Op::Register {
            service: arg(&args, 1)?.to_owned(),
        }),
        "assign" => encode(&egress::Op::Assign {
            service: arg(&args, 1)?.to_owned(),
            nodes: args[2..]
                .iter()
                .map(hex::decode)
                .collect::<std::result::Result<_, _>>()?,
        }),
        "set-model" => encode(&harness::Op::SetModel {
            id: arg(&args, 1)?.to_owned(),
            wire: harness::Wire::AnthropicMessages,
            service: arg(&args, 2)?.to_owned(),
            name: arg(&args, 3)?.to_owned(),
        }),
        "config" => {
            let thinking = match arg(&args, 3)? {
                "none" => None,
                "off" => Some(harness::config::Thinking::Off),
                "low" => Some(harness::config::Thinking::Low),
                "medium" => Some(harness::config::Thinking::Medium),
                "high" => Some(harness::config::Thinking::High),
                "max" => Some(harness::config::Thinking::Max),
                other => {
                    return Err(
                        format!("thinking is none, off, low, medium, high or max: {other}").into(),
                    );
                }
            };
            let server_tools = match arg(&args, 4)? {
                "web" => vec![
                    harness::config::ServerTool::WebSearch,
                    harness::config::ServerTool::WebFetch,
                ],
                _ => Vec::new(),
            };
            let optional = |at: usize| -> Result<Option<u64>> {
                let value = number(&args, at)?;
                Ok((value > 0).then_some(value))
            };
            let config = harness::Config {
                model: arg(&args, 1)?.to_owned(),
                max_tokens: arg(&args, 2)?.parse()?,
                wait: None,
                thinking,
                server_tools,
                listen: Vec::new(),
                settle: optional(5)?,
                compact_at: optional(6)?,
                system: text(arg(&args, 7)?)?.trim_end().to_owned(),
            };
            print!("{}", config.write());
            Ok(())
        }
        "commit-file" => encode(&files::Op::Commit {
            base: None,
            message: arg(&args, 1)?.to_owned(),
            changes: vec![files::Change::Put {
                path: arg(&args, 2)?.to_owned(),
                exec: false,
                meta: Default::default(),
                content: files::Content::Chunks {
                    size: number(&args, 3)?,
                    chunks: args[4..]
                        .iter()
                        .map(|id| blob_id(id))
                        .collect::<Result<_>>()?,
                },
            }],
        }),
        "set-list" => encode(&access::Op::Set {
            program: arg(&args, 1)?.to_owned(),
            key: arg(&args, 2)?.to_owned(),
            list: list(arg(&args, 3)?)?,
        }),
        "agent-key" => {
            println!("{}", harness::agent_key(number(&args, 1)?));
            Ok(())
        }
        "conversation-key" => {
            println!("{}", harness::conversation_key(number(&args, 1)?));
            Ok(())
        }
        "post" => encode(&chat::Op::PostMessage {
            channel_id: arg(&args, 1)?.to_owned(),
            message_id: arg(&args, 2)?.to_owned(),
            blocks: chat::parse_message(&text(arg(&args, 3)?)?),
            thread: None,
        }),
        "open-dm" => encode(&chat::Op::CreateDmChannel {
            counterpart: number(&args, 1)?,
            name: arg(&args, 2)?.to_owned(),
        }),
        "add-key" => consent(&args),
        "q-generation" => encode(&identity::Query::Generation {
            key: hex::decode(arg(&args, 1)?)?,
        }),
        "commit-files" => files_commit(&args),
        "q-files-head" => encode(&files::Query::Head),
        "q-find" => encode(&files::Query::Find {
            prefix: arg(&args, 1)?.to_owned(),
            at: None,
            page: page(args.get(2))?,
        }),
        "q-read" => encode(&files::Query::Read {
            path: arg(&args, 1)?.to_owned(),
            at: None,
            offset: number(&args, 2)?,
            len: number(&args, 3)?,
        }),
        "d-files-head" => {
            let files::Reply::Head(head) = borsh::from_slice(&stdin()?)? else {
                return Err("not a head".into());
            };
            match head {
                Some(row) => println!("{}", hex::encode(row.id.digest())),
                None => println!("none"),
            }
            Ok(())
        }
        "d-find" => {
            let files::Reply::Listing { entries, .. } = borsh::from_slice(&stdin()?)? else {
                return Err("not a listing".into());
            };
            for entry in entries.items {
                let size = match entry.node {
                    files::Node::File { size, .. } => size.to_string(),
                    _ => "-".to_owned(),
                };
                println!("{}\t{}\t{}", entry.node.kind(), size, entry.path);
            }
            if let Some(next) = entries.next {
                println!("next\t{}", hex::encode(next));
            }
            Ok(())
        }
        "d-read" => {
            let files::Reply::Contents(contents) = borsh::from_slice(&stdin()?)? else {
                return Err("not contents".into());
            };
            eprintln!(
                "size {} offset {} len {} eof {}",
                contents.size,
                contents.offset,
                contents.bytes.len(),
                contents.eof
            );
            std::io::stdout().write_all(&contents.bytes)?;
            Ok(())
        }
        "q-of-key" => encode(&identity::Query::OfKey {
            key: hex::decode(arg(&args, 1)?)?,
        }),
        "q-get" => encode(&identity::Query::Get {
            number: number(&args, 1)?,
        }),
        "read-budgets" => encode(&harness::Op::ReadBudgets),
        "q-budget" => encode(&harness::Query::Budget),
        "q-model" => encode(&harness::Query::Model {
            id: arg(&args, 1)?.to_owned(),
        }),
        "q-threads" => encode(&harness::Query::Threads {
            agent: number(&args, 1)?,
            page: page(args.get(2))?,
        }),
        "q-agent" => encode(&harness::Query::Agent {
            agent: number(&args, 1)?,
        }),
        "q-services" => encode(&egress::Query::Services {
            page: PageRequest {
                after: None,
                limit: None,
            },
        }),
        "q-serving" => encode(&egress::Query::Serving {
            service: arg(&args, 1)?.to_owned(),
        }),
        "q-list" => encode(&access::Query::List {
            program: arg(&args, 1)?.to_owned(),
            key: arg(&args, 2)?.to_owned(),
        }),
        "d-identity" => {
            let reply: identity::Reply = borsh::from_slice(&stdin()?)?;
            println!("{reply:#?}");
            Ok(())
        }
        "d-harness" => {
            let reply: harness::Reply = borsh::from_slice(&stdin()?)?;
            println!("{reply:#?}");
            Ok(())
        }
        "d-egress" => {
            let reply: egress::Reply = borsh::from_slice(&stdin()?)?;
            println!("{reply:#?}");
            Ok(())
        }
        "d-access" => {
            let reply: access::Reply = borsh::from_slice(&stdin()?)?;
            println!("{reply:#?}");
            Ok(())
        }
        "d-number" => {
            let number: u64 = borsh::from_slice(&hex::decode(arg(&args, 1)?)?)?;
            println!("{number}");
            Ok(())
        }
        other => Err(format!("unknown verb {other}").into()),
    }
}
