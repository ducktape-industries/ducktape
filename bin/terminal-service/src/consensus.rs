//! Shared terminal commands decoded from committed Chat state.
//! Channel ownership and message authorship retain the existing terminal rules.
/// the chat channel that carries a session's ordered command lane. NON-colon by
/// necessity (see the module doc): a live node can only author `User`-origin
/// ids, and chat reserves colon ids to module/system origins. The 16-hex
/// session id keeps the pattern precise, so the app's hide predicate matches
/// exactly these and never a member's own `term-*` channel.
pub fn session_channel(session_id: &str) -> String {
    format!("term-{session_id}")
}

/// encode a submitted command line as the chat message body a member posts: a
/// single plain paragraph. Inverse of [`command_text`].
pub fn command_blocks(text: &str) -> Vec<chat::Block> {
    vec![chat::Block::paragraph(text)]
}

/// decode a committed chat message body back to the command line fed to the pty
/// — the inverse of [`command_blocks`]. Flattens paragraph/quote spans and code
/// text (a command is one plain paragraph, but be liberal in what we accept),
/// joining blocks with newlines so a pasted multi-line command survives.
pub fn command_text(blocks: &[chat::Block]) -> String {
    fn spans(out: &mut String, spans: &[chat::Span]) {
        for span in spans {
            out.push_str(&span.text);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let mut piece = String::new();
        match block {
            chat::Block::Paragraph(s) | chat::Block::Quote(s) => spans(&mut piece, s),
            chat::Block::Code { text, .. } => piece.push_str(text),
            chat::Block::Divider => {}
        }
        parts.push(piece);
    }
    parts.join("\n")
}

/// The canonical author displayed beside an accepted terminal command.
fn render_author(author: &chat::Party) -> String {
    match author {
        chat::Party::Key(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        chat::Party::Account(number) => format!("acct:{number}"),
        chat::Party::Module(id) => format!("module:{id}"),
        chat::Party::System => "system".to_string(),
    }
}

/// one committed command ready for the pty: the verified `origin` and the
/// decoded `text`. Deliberately does NOT carry the seq — the projector owns the
/// per-session cursor.
#[derive(Debug, PartialEq, Eq)]
pub struct Projected {
    pub origin: String,
    pub text: String,
}

/// Account owners share their account's authority. A historical key owner
/// still requires the actual signer of the post, even after it joins an account.
/// Module and system owners never grant authority over the operator's PTY.
fn author_is_owner(head: &chat::MessageHead, owner: &chat::Party) -> bool {
    match owner {
        chat::Party::Account(number) => head.author == chat::Party::Account(*number),
        chat::Party::Key(owner_key) => {
            matches!(&head.content_origin, sdk::Origin::External(key) if key == owner_key)
        }
        chat::Party::Module(_) | chat::Party::System => false,
    }
}

/// the drive-or-refuse decision for one committed message — the unit-testable
/// core of the projector. `Err` is a stable snake_case reason the caller logs;
/// it advances its cursor either way, so a refused post is skipped, never
/// retried. Two refusals:
///
/// - `command_deleted` — the message is tombstoned (content and reactions are
///   cleared; running an empty redaction would be wrong).
/// - `command_not_channel_owner` — the verified author is not this channel's
///   owner, i.e. not the node that owns the pty. THIS is the gate: the channel
///   is open to post to, and open to read, but only its owner drives.
pub fn project_message(
    view: &chat::MessageView,
    owner: &chat::Party,
) -> Result<Projected, &'static str> {
    let is_tombstone = view.head.deleted;
    if is_tombstone {
        return Err("command_deleted");
    }
    let from_owner = author_is_owner(&view.head, owner);
    if !from_owner {
        return Err("command_not_channel_owner");
    }
    Ok(Projected {
        origin: render_author(&view.head.author),
        text: command_text(&view.head.blocks),
    })
}

/// A missing channel is unreadable. Module/system ownership supplies no
/// operator authority, so it is treated as unowned by the terminal lane.
pub fn channel_owner(channel: Option<chat::Channel>) -> Result<chat::Party, &'static str> {
    let Some(channel) = channel else {
        return Err("channel_unreadable");
    };
    match channel.owner {
        owner @ (chat::Party::Account(_) | chat::Party::Key(_)) => Ok(owner),
        chat::Party::Module(_) | chat::Party::System => Err("channel_unowned"),
    }
}

/// Project public, committed Chat messages into one live shared session. The
/// expected owner comes from successful channel creation/ownership confirmation.
/// The caller owns this future for the session lifetime; service shutdown must
/// stop the runtime, which cancels an outstanding node query here.
pub async fn project(
    runtime: &crate::runtime::Runtime,
    client: &ducktape_rpc::Client,
    session: &str,
    caller: &crate::state::Caller,
    owner: &chat::Party,
) -> Result<(), String> {
    let changes = runtime.changes();
    let result = tokio::select! {
        ended = wait_ended(runtime, session, caller, changes) => return ended,
        result = feed(runtime, client, session, caller, owner) => result,
    };
    // Query/protocol failures have no native input fallback. The runtime closes
    // asynchronously while continuing to drain executor output.
    let _ = runtime.close(session.into(), caller.clone()).await;
    result
}

async fn wait_ended(
    runtime: &crate::runtime::Runtime,
    session: &str,
    caller: &crate::state::Caller,
    mut changes: tokio::sync::watch::Receiver<()>,
) -> Result<(), String> {
    loop {
        let status = runtime.status(session.into(), caller.clone()).await?;
        if status.ended {
            return Ok(());
        }
        if changes.changed().await.is_err() {
            return Ok(());
        }
    }
}

async fn feed(
    runtime: &crate::runtime::Runtime,
    client: &ducktape_rpc::Client,
    session: &str,
    caller: &crate::state::Caller,
    owner: &chat::Party,
) -> Result<(), String> {
    let channel = session_channel(session);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let status = runtime.status(session.into(), caller.clone()).await?;
        if status.ended {
            return Ok(());
        }
        let reply: chat::ChatReply = client
            .query(
                "chat",
                &chat::ChatQuery::Channel {
                    channel_id: channel.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let chat::ChatReply::Channel(Some(record)) = reply else {
            return Err("terminal channel unreadable".into());
        };
        let matching_channel = record.id == channel;
        let matching_owner = &channel_owner(Some(record))? == owner;
        if !matching_channel || !matching_owner {
            return Err("terminal channel owner changed".into());
        }
        let from_seq = status
            .command_cursor
            .checked_add(1)
            .ok_or("terminal command cursor exhausted")?;
        let reply: chat::ChatReply = client
            .query(
                "chat",
                &chat::ChatQuery::MessagesRange {
                    channel_id: channel.clone(),
                    from_seq,
                    limit: chat::MAX_QUERY_LIMIT,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let chat::ChatReply::Messages(views) = reply else {
            return Err("unexpected terminal messages reply".into());
        };
        runtime
            .committed(session.into(), caller.clone(), owner.clone(), views)
            .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat::{Block, MessageHead, MessageView, Party, Span};
    fn view(seq: u64, author: Party, blocks: Vec<Block>, deleted: bool) -> MessageView {
        let origin = match &author {
            Party::Account(number) => sdk::Origin::Program(*number),
            Party::Key(key) => sdk::Origin::External(key.clone()),
            Party::Module(module) => sdk::Origin::Module(module.clone()),
            Party::System => sdk::Origin::System,
        };
        MessageView {
            channel_id: "term-0000000000000001".into(),
            seq,
            head: MessageHead {
                message_id: format!("m{seq}"),
                origin: origin.clone(),
                content_origin: origin,
                author,
                revision: 1,
                blocks,
                created_at: 0,
                rev: 0,
                edited_at: None,
                base_rev: None,
                deleted,
                thread: None,
                reply_count: 0,
                last_reply_seq: None,
            },
        }
    }

    #[test]
    fn session_channel_is_non_colon_and_prefixed() {
        let ch = session_channel("00000000deadbeef");
        assert_eq!(ch, "term-00000000deadbeef");
        // the whole point of the scheme: a live node can author this, unlike a
        // colon id, and the app hide predicate keys off exactly this shape.
        assert!(!ch.contains(':'));
    }

    #[test]
    fn command_blocks_and_text_round_trip() {
        let line = "cargo test -p noded";
        let blocks = command_blocks(line);
        assert_eq!(command_text(&blocks), line);
        // a single plain paragraph is the exact wire a member posts.
        assert!(matches!(blocks.as_slice(), [Block::Paragraph(_)]));
    }

    #[test]
    fn command_text_flattens_multi_span_and_multi_block() {
        // liberal decode: multiple spans concatenate, blocks join by newline.
        let blocks = vec![
            Block::Paragraph(vec![Span::plain("echo "), Span::plain("hi")]),
            Block::Code {
                lang: None,
                text: "ls -la".into(),
            },
        ];
        assert_eq!(command_text(&blocks), "echo hi\nls -la");
    }

    /// the channel owner every test below gates against — the host node that
    /// created the session's channel.
    const HOST: [u8; 2] = [0xab, 0xcd];

    fn channel(owner: Option<Vec<u8>>) -> chat::Channel {
        chat::Channel {
            id: "term-0000000000000001".into(),
            name: "term-0000000000000001".into(),
            created_at: 0,
            head_seq: 0,
            post_policy: chat::PostPolicy::Open,
            hooks: Vec::new(),
            pinned: Vec::new(),
            huddle: Vec::new(),
            voice: false,
            owner: owner.map_or(Party::System, Party::Key),
            revision: 1,
            archived: false,
        }
    }

    #[test]
    fn the_channel_owner_drives_the_pty() {
        let projected = project_message(
            &view(1, Party::Key(HOST.into()), command_blocks("pwd"), false),
            &Party::Key(HOST.into()),
        )
        .expect("the owner's command projects");
        assert_eq!(projected.text, "pwd");
        // the verified User author renders to hex — a spoof-proof identity, not
        // a caller string (spec finding #5).
        assert_eq!(projected.origin, "abcd");
    }

    #[test]
    fn any_other_member_is_refused_at_the_pty() {
        // THE HOLE THIS GATE CLOSES: the command channel is `PostPolicy::Open`,
        // so an admitted member's post commits exactly like the owner's — it is
        // signed, ordered, durable and indistinguishable at the chat layer. The
        // projector is the only thing between it and a live pty spending the
        // host's own subscription. Mutating `author_is_owner` to `true` reddens
        // this and nothing else.
        let stranger = Party::Key(vec![0x99, 0x99]);
        assert_eq!(
            project_message(
                &view(1, stranger, command_blocks("rm -rf /"), false),
                &Party::Key(HOST.into())
            ),
            Err("command_not_channel_owner"),
        );
    }

    #[test]
    fn a_non_key_origin_cannot_use_a_historical_key_grant() {
        // A program account does not inherit the controller's historical key
        // grant. Module and system messages do not carry signing-key evidence.
        for author in [
            Party::Module("chat".into()),
            Party::Account(7),
            Party::System,
        ] {
            assert!(!author_is_owner(
                &view(1, author, command_blocks("pwd"), false).head,
                &Party::Key(HOST.into())
            ));
        }
    }

    #[test]
    fn historical_key_ownership_requires_the_original_signer_after_admission() {
        let mut post = view(1, Party::Account(7), command_blocks("pwd"), false);
        let owner = Party::Key(HOST.into());
        post.head.origin = sdk::Origin::External(HOST.into());
        post.head.content_origin = post.head.origin.clone();
        let projected = project_message(&post, &owner).expect("the original key retains its grant");
        assert_eq!(
            projected.origin, "acct:7",
            "display retains the actual account author"
        );
        for origin in [
            sdk::Origin::External(vec![99]),
            sdk::Origin::Program(7),
            sdk::Origin::Module("runs".into()),
        ] {
            post.head.content_origin = origin;
            post.head.edited_at = Some(1);
            assert_eq!(
                project_message(&post, &owner),
                Err("command_not_channel_owner")
            );
        }
    }

    #[test]
    fn account_ownership_accepts_only_its_canonical_author() {
        let owner = Party::Account(7);
        for key in [HOST.to_vec(), vec![99]] {
            let mut post = view(1, Party::Account(7), command_blocks("pwd"), false);
            post.head.origin = sdk::Origin::External(key);
            post.head.content_origin = post.head.origin.clone();
            assert!(project_message(&post, &owner).is_ok());
        }
        for author in [
            Party::Account(8),
            Party::Key(HOST.into()),
            Party::Module("chat".into()),
            Party::System,
        ] {
            assert_eq!(
                project_message(&view(1, author, command_blocks("pwd"), false), &owner),
                Err("command_not_channel_owner")
            );
        }
        for owner in [Party::Module("chat".into()), Party::System] {
            assert_eq!(
                project_message(
                    &view(1, owner.clone(), command_blocks("pwd"), false),
                    &owner
                ),
                Err("command_not_channel_owner")
            );
        }
    }

    #[test]
    fn a_tombstone_is_refused_even_from_the_owner() {
        // a deleted (redacted) message must NOT run: its content is cleared, and
        // running an empty redaction would be wrong.
        assert_eq!(
            project_message(
                &view(2, Party::Key(HOST.into()), Vec::new(), true),
                &Party::Key(HOST.into())
            ),
            Err("command_deleted"),
        );
    }

    #[test]
    fn a_projector_with_no_owner_to_gate_on_refuses_to_start() {
        // fail closed, both ways: no channel record and an unowned channel each
        // yield a named refusal, never an owner the gate would compare against.
        assert_eq!(channel_owner(None), Err("channel_unreadable"));
        assert_eq!(channel_owner(Some(channel(None))), Err("channel_unowned"));
        assert_eq!(
            channel_owner(Some(channel(Some(HOST.into())))),
            Ok(Party::Key(HOST.to_vec())),
        );
    }

    #[test]
    fn committed_pages_validate_before_advancing_and_never_repeat_consumed_posts() {
        let caller = crate::state::Caller {
            account: 7,
            node: [1; 32],
        };
        let mut sessions = crate::state::Sessions::default();
        let id = "0000000000000001";
        sessions
            .insert(id.into(), caller.clone(), crate::state::Mode::Shared)
            .unwrap();
        sessions.created(id);
        let owner = Party::Account(7);
        let first = view(1, owner.clone(), command_blocks("first"), false);
        let mut wrong = view(2, owner.clone(), command_blocks("second"), false);
        wrong.channel_id = "another-channel".into();
        assert!(
            sessions
                .commands(id, &caller, &owner, &[first.clone(), wrong])
                .is_err()
        );
        assert_eq!(
            sessions
                .commands(id, &caller, &owner, std::slice::from_ref(&first))
                .unwrap()
                .len(),
            1
        );
        assert!(
            sessions
                .commands(id, &caller, &owner, &[first])
                .unwrap()
                .is_empty()
        );
        let deleted = view(2, owner.clone(), command_blocks("deleted"), true);
        let stranger = view(3, Party::Account(8), command_blocks("stranger"), false);
        assert!(
            sessions
                .commands(id, &caller, &owner, &[deleted, stranger])
                .unwrap()
                .is_empty()
        );
        let edited = view(3, owner.clone(), command_blocks("changed author"), false);
        assert!(
            sessions
                .commands(id, &caller, &owner, &[edited])
                .unwrap()
                .is_empty()
        );
        let fourth = view(4, owner.clone(), command_blocks("fourth"), false);
        assert!(
            sessions
                .commands(id, &caller, &owner, &[fourth.clone(), fourth.clone()])
                .is_err()
        );
        assert_eq!(
            sessions
                .commands(id, &caller, &owner, &[fourth])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn render_author_covers_every_kind() {
        assert_eq!(render_author(&Party::Key(vec![0x01, 0xff])), "01ff");
        assert_eq!(render_author(&Party::Account(7)), "acct:7");
        assert_eq!(render_author(&Party::Module("chat".into())), "module:chat");
        assert_eq!(render_author(&Party::System), "system");
    }
}
