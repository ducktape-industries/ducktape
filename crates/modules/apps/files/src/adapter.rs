//! Native and guest writes share identity resolution, pure operations and
//! source-owned attribution publication. The host commits their effects together.

use attribution::{AttributionMsg, ObjectRef, Reason, Relation};
use duckfs_core::{
    Actor, Authority, FilesMsg, FilesWriteOutput, Fs, Kind, ObjectStore, PUTBLOB_FRAME_TAG, Refs,
    WriteOutcome, decode_msg, encode_write_output, to_hex,
};
use sdk::{Ctx, Error, Msg, Origin};

async fn identity_account(
    ctx: &dyn Ctx,
    query: identity::IdentityQuery,
) -> Result<Option<identity::AccountView>, Error> {
    let bytes = ctx
        .query("identity", &identity::encode_query(&query))
        .await?;
    let reply =
        identity::decode_reply(&bytes).map_err(|e| Error::module("identity_reply_decode", e))?;
    let identity::IdentityReply::Account(account) = reply else {
        return Err(Error::module(
            "unexpected_identity_reply",
            "files: unexpected identity reply",
        ));
    };
    Ok(account)
}

async fn authority(ctx: &dyn Ctx) -> Result<Authority, Error> {
    match &ctx.env().origin {
        Origin::External(key) => {
            if key.is_empty() {
                return Err(Error::module(
                    "empty_origin_key",
                    "files: external key is empty",
                ));
            }
            let account =
                identity_account(ctx, identity::IdentityQuery::OfKey { key: key.clone() }).await?;
            Ok(Authority::External {
                key: key.clone(),
                account: account.map(|account| account.number),
            })
        }
        Origin::Program(number) => {
            let account = identity_account(ctx, identity::IdentityQuery::Get { number: *number })
                .await?
                .ok_or_else(|| {
                    Error::module("unknown_account", "files: program account does not exist")
                })?;
            let active_program = matches!(
                account.control,
                identity::Control::Program {
                    standing: identity::ProgramStanding::Active,
                    ..
                }
            );
            if !active_program {
                return Err(Error::module(
                    "not_an_active_program",
                    "files: program account is not active",
                ));
            }
            Ok(Authority::Program(account.number))
        }
        Origin::Module(module) => Ok(Authority::Module(module.clone())),
        Origin::System => Ok(Authority::System),
    }
}

fn attributed_actor(actor: &Actor) -> attribution::Actor {
    match actor {
        Actor::Account(account) => attribution::Actor::Account(*account),
        Actor::Key(key) => attribution::Actor::Key(key.clone()),
        Actor::Module(module) => attribution::Actor::Module(module.clone()),
        Actor::System => attribution::Actor::System,
    }
}

fn relation(owner: &Actor, reason: Reason, detail: Vec<u8>) -> Vec<Relation> {
    match owner {
        Actor::Account(recipient) => vec![Relation {
            recipient: *recipient,
            reason,
            detail,
        }],
        Actor::Key(_) | Actor::Module(_) | Actor::System => Vec::new(),
    }
}

fn publish(
    ctx: &mut dyn Ctx,
    actor: &Actor,
    revision: u64,
    kind: &str,
    object: String,
    relations: Vec<Relation>,
) {
    ctx.emit_msg(Msg {
        target: "attribution".into(),
        payload: attribution::encode_msg(&AttributionMsg::Attribute {
            object: ObjectRef {
                kind: kind.into(),
                object,
            },
            revision,
            actor: attributed_actor(actor),
            relations,
            transfers: Vec::new(),
        }),
    });
}

fn publish_changes(ctx: &mut dyn Ctx, actor: &Actor, before: &Refs, after: &Refs) {
    let new_snapshot = after.head != before.head;
    if new_snapshot {
        let snapshot = after.head.expect("a commit sets the head");
        publish(
            ctx,
            actor,
            after.source_revision,
            "snapshot",
            to_hex(&snapshot),
            relation(actor, Reason::Authorship, Vec::new()),
        );
    }
    for (name, pin) in &after.pins {
        let changed_pin = before.pins.get(name) != Some(pin);
        if changed_pin {
            publish(
                ctx,
                actor,
                after.source_revision,
                "pin",
                to_hex(name.as_bytes()),
                relation(&pin.owner, Reason::Ownership, pin.snapshot.to_vec()),
            );
        }
    }
    for name in before.pins.keys() {
        if !after.pins.contains_key(name) {
            publish(
                ctx,
                actor,
                after.source_revision,
                "pin",
                to_hex(name.as_bytes()),
                Vec::new(),
            );
        }
    }
}

pub(crate) async fn apply_op<S: ObjectStore>(
    fs: &mut Fs<S>,
    ctx: &mut dyn Ctx,
    payload: &[u8],
) -> Result<(), Error> {
    let authority = authority(ctx).await?;
    let actor = authority.actor();
    let env = ctx.env().clone();
    let before = fs.pending_refs().clone();
    let outcome = match payload.first() {
        Some(&PUTBLOB_FRAME_TAG) => {
            fs.putblob(&authority, env.height, &payload[1..])
                .map_err(|e| Error::module("files_putblob", e))?;
            WriteOutcome::PutBlob {
                chunk: to_hex(&duckfs_core::objects::object_id(Kind::Chunk, &payload[1..])),
            }
        }
        _ => match decode_msg(payload).map_err(|e| Error::module("codec", e))? {
            FilesMsg::Commit {
                base_snapshot,
                message,
                changes,
            } => {
                let notifications = fs
                    .commit(
                        &authority,
                        env.height,
                        env.consensus_time,
                        base_snapshot,
                        message,
                        changes,
                    )
                    .map_err(|e| Error::module(e.class(), e.to_string()))?;
                for notification in notifications {
                    ctx.emit_msg(Msg {
                        target: notification.module_id.clone(),
                        payload: notification.payload(),
                    });
                }
                WriteOutcome::Commit {
                    snapshot: to_hex(&fs.pending_refs().head.expect("commit sets head")),
                }
            }
            FilesMsg::ProjectSnapshot { snapshot, path } => {
                let snapshot = fs
                    .project_snapshot(&authority, env.height, env.consensus_time, snapshot, path)
                    .map_err(|e| Error::module("files_project_snapshot", e))?;
                WriteOutcome::ProjectSnapshot { snapshot }
            }
            FilesMsg::CompareExchangeRetention {
                key,
                expected,
                replacement,
            } => {
                fs.compare_exchange_retention(
                    &authority,
                    env.height,
                    key.clone(),
                    expected,
                    replacement.clone(),
                )
                .map_err(|e| Error::module("files_compare_exchange_retention", e))?;
                WriteOutcome::CompareExchangeRetention {
                    key,
                    reference: replacement,
                }
            }
            FilesMsg::Pin { snapshot, name } => {
                fs.pin(&authority, env.height, snapshot.clone(), name.clone())
                    .map_err(|e| Error::module("files_pin", e))?;
                let pin = fs.pending_refs().pins.get(&name).expect("pin inserted");
                WriteOutcome::Pin {
                    snapshot: to_hex(&pin.snapshot),
                    name,
                }
            }
            FilesMsg::Unpin { name } => {
                fs.unpin(&authority, env.height, name.clone())
                    .map_err(|e| Error::module("files_unpin", e))?;
                WriteOutcome::Unpin { name }
            }
            FilesMsg::Watch { prefix, module_id } => {
                fs.watch(&authority, env.height, prefix.clone(), module_id.clone())
                    .map_err(|e| Error::module("files_watch", e))?;
                WriteOutcome::Watch { prefix, module_id }
            }
            FilesMsg::Unwatch { prefix, module_id } => {
                fs.unwatch(&authority, env.height, prefix.clone(), module_id.clone())
                    .map_err(|e| Error::module("files_unwatch", e))?;
                WriteOutcome::Unwatch { prefix, module_id }
            }
        },
    };
    let after = fs.pending_refs();
    publish_changes(ctx, &actor, &before, after);
    let output = encode_write_output(&FilesWriteOutput {
        actor,
        source_revision: after.source_revision,
        outcome,
    });
    // Generic action receipts keep the assigned stamp, not raw output bytes.
    // Publish minted object IDs there as well; this never includes file bodies.
    ctx.set_assigned(output.clone());
    ctx.set_output(output);
    Ok(())
}
