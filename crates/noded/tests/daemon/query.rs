// Queries over HTTP: unsigned ones by the network's policy, and watched ones pushed per block.

use super::*;

#[test]
fn an_unsigned_query_reads_as_no_key_and_is_answered_only_while_the_network_opens_them() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    let text = std::fs::read_to_string(&founding).unwrap();
    std::fs::write(&founding, format!("queries = \"open\"\n{text}")).unwrap();
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let account = abi::encode(&role::identity::Query::Account(Vec::new()));
        let answer = client
            .query_unsigned(Layer::Confirmed, "identity", account)
            .await
            .unwrap();
        assert_eq!(
            abi::decode::<role::identity::Reply>(&answer).unwrap(),
            role::identity::Reply::Account(None),
            "the empty key holds no account"
        );
        let env = abi::encode(&vec![Step::Env]);
        let answer = client
            .query_unsigned(Layer::Confirmed, "probe", env.clone())
            .await
            .unwrap();
        let replies: Vec<Reply> = abi::decode(&answer).unwrap();
        let [Reply::Env(seen)] = replies.as_slice() else {
            panic!("the probe answers its env: {replies:?}");
        };
        assert_eq!(seen.origin, abi::Origin::unsigned());
        assert_eq!(seen.origin, abi::Origin::External(Vec::new()));
        assert_eq!(seen.sender, None);

        let closing = Motion::Queries {
            policy: QueryPolicy::Signed,
        };
        membership(&client, &seat.identity, Op::Vote(closing)).await;
        let unsigned = client
            .query_unsigned(Layer::Confirmed, "probe", env.clone())
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = unsigned else {
            panic!("an unsigned query is refused once the vote closes them: {unsigned}");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);
        let signed = query_frame(&client, &seat.identity, "probe", env).await;
        client.query(Layer::Confirmed, signed).await.unwrap();
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

async fn watch_send<S>(socket: &mut S, watch: noded::wire::Watch)
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    let message = tokio_tungstenite::tungstenite::Message::binary(abi::encode(&watch));
    socket.send(message).await.unwrap();
}

async fn watch_next<S>(socket: &mut S) -> noded::wire::Pushed
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    loop {
        let message = socket.next().await.unwrap().unwrap();
        if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message {
            return abi::decode(&bytes).unwrap();
        }
    }
}

const METERED_WITH_FUEL_ENOUGH_FOR_ANY_TEST_QUERY: u64 = u64::MAX;

fn metered() -> LoadConfig {
    LoadConfig {
        query_fuel: std::num::NonZeroU64::new(METERED_WITH_FUEL_ENOUGH_FOR_ANY_TEST_QUERY),
        ..LoadConfig::default()
    }
}

async fn query_steps(client: &Client, key: &ed25519::PrivateKey, steps: Vec<Step>) -> Vec<u8> {
    query_frame(client, key, "probe", abi::encode(&steps)).await
}

fn aged(frame: &[u8]) -> u64 {
    Frame::decode(frame).unwrap().body.anchor.height + host::ANCHOR_WINDOW
}

async fn watch_block_writing<S>(socket: &mut S, key: &[u8], last: u64) -> noded::wire::Pushed
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let height = match message {
            Message::Ping(echo) => u64::from_le_bytes(echo[..].try_into().unwrap()),
            Message::Binary(bytes) => {
                let pushed: noded::wire::Pushed = abi::decode(&bytes).unwrap();
                let noded::wire::Pushed::Block {
                    height,
                    writes,
                    answers,
                    ..
                } = &pushed
                else {
                    panic!("an answer came that no ask asked for: {pushed:?}");
                };
                let wrote = writes.iter().any(|(written, _)| written == key);
                let quiet = answers.is_empty();
                let at = *height;
                if wrote {
                    return pushed;
                }
                assert!(
                    quiet,
                    "a block that wrote nothing a query read ran it again: {pushed:?}"
                );
                at
            }
            other => panic!("the watch socket sent {other:?}"),
        };
        assert!(
            height < last,
            "no block wrote {key:?} before the frame's anchor aged out at {last}"
        );
    }
}

#[test]
fn a_watched_query_is_answered_at_once_and_again_only_with_a_block_that_writes_what_it_read() {
    apart!();
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start_with(metered());
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let reads_a = vec![Step::Op(HostOp::Get(b"a".to_vec())), Step::Env];
        let asked = query_steps(&client, &alice, reads_a).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 1,
                frame: asked,
            },
        )
        .await;
        let answer = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 1,
            outcome: Watched::Answered(bytes),
            ..
        } = &answer
        else {
            panic!("the ask is answered first: {answer:?}");
        };
        let replies: Vec<Reply> = abi::decode(bytes).unwrap();
        assert_eq!(replies[0], Reply::Host(HostReply::Value(None)));
        let failing = query_steps(&client, &alice, vec![Step::Fail("asked to".into())]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 2,
                frame: failing,
            },
        )
        .await;
        let failed = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 2,
            outcome: Watched::Refused(refusal),
            ..
        } = &failed
        else {
            panic!("a program's refusal answers the ask: {failed:?}");
        };
        assert_eq!(refusal.reason, "probe");

        let beside = frame(&client, &alice, vec![set(b"b", b"1")]).await;
        let last = aged(&beside);
        client.submit(beside, Preconfirm::Off).await.unwrap();
        let wrote_b = watch_block_writing(&mut socket, b"b", last).await;
        let Pushed::Block { answers, .. } = &wrote_b else {
            panic!("{wrote_b:?}");
        };
        assert!(answers.is_empty(), "{wrote_b:?}");

        let read = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let last = aged(&read);
        client.submit(read, Preconfirm::Off).await.unwrap();
        let wrote_a = watch_block_writing(&mut socket, b"a", last).await;
        let Pushed::Block {
            height,
            writes,
            answers,
            ..
        } = &wrote_a
        else {
            panic!("{wrote_a:?}");
        };
        assert_eq!(writes, &vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        let [(1, Watched::Answered(bytes))] = answers.as_slice() else {
            panic!("the block that wrote what the query read carries its answer: {wrote_a:?}");
        };
        let replies: Vec<Reply> = abi::decode(bytes).unwrap();
        assert_eq!(
            replies[0],
            Reply::Host(HostReply::Value(Some(b"1".to_vec())))
        );
        let Reply::Env(env) = &replies[1] else {
            panic!("{replies:?}");
        };
        assert_eq!(env.height, *height, "the answer is the state at its block");

        watch_send(&mut socket, Watch::Drop { id: 1 }).await;
        let again = frame(&client, &alice, vec![set(b"a", b"2")]).await;
        let last = aged(&again);
        client.submit(again, Preconfirm::Off).await.unwrap();
        let dropped = watch_block_writing(&mut socket, b"a", last).await;
        let Pushed::Block { answers, .. } = &dropped else {
            panic!("{dropped:?}");
        };
        assert!(answers.is_empty(), "a dropped query still ran: {dropped:?}");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_watch_ask_signed_as_a_query_runs_as_its_signer_and_the_same_frame_is_no_op() {
    apart!();
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start_with(metered());
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let asked = query_steps(&client, &alice, vec![Step::Env]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 1,
                frame: asked.clone(),
            },
        )
        .await;
        let answer = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 1,
            outcome: Watched::Answered(bytes),
            ..
        } = &answer
        else {
            panic!("an ask signed under the query namespace is answered: {answer:?}");
        };
        let replies: Vec<Reply> = abi::decode(bytes).unwrap();
        let [Reply::Env(seen)] = replies.as_slice() else {
            panic!("the probe answers its env: {replies:?}");
        };
        let signer = alice.public_key().as_ref().to_vec();
        assert_eq!(seen.origin, abi::Origin::External(signer));
        let Err(noded::Error::Refused(refusal)) = client.submit(asked, Preconfirm::Off).await
        else {
            panic!("a standing ask's frame was taken as an op");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_watch_ask_signed_as_an_op_is_refused_so_no_standing_ask_doubles_as_a_transaction() {
    apart!();
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start_with(metered());
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let as_op = frame(&client, &alice, vec![Step::Env]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 1,
                frame: as_op,
            },
        )
        .await;
        let answer = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 1,
            outcome: Watched::Refused(refusal),
            ..
        } = &answer
        else {
            panic!("an ask signed under the op namespace is refused: {answer:?}");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        assert_eq!(
            refusal.sentence,
            "the proof does not bind the frame to its signer"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_watch_ask_unsigned_or_on_a_network_that_meters_no_fuel_is_refused_and_a_frame_that_is_no_watch_ends_the_socket()
 {
    apart!();
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let unsigned = Watch::Ask {
            id: 1,
            frame: vec![1, 2, 3],
        };
        watch_send(&mut socket, unsigned).await;
        let unverified = watch_next(&mut socket).await;
        assert!(
            matches!(
                unverified,
                Pushed::Answer {
                    id: 1,
                    outcome: Watched::Refused(_),
                    ..
                }
            ),
            "{unverified:?}"
        );
        let unmetered = query_steps(&client, &alice, vec![Step::Env]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 2,
                frame: unmetered,
            },
        )
        .await;
        let refused = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 2,
            outcome: Watched::Refused(refusal),
            ..
        } = &refused
        else {
            panic!("{refused:?}");
        };
        assert_eq!(refusal.reason, reason::UNSUPPORTED);
        let after = query_steps(&client, &alice, vec![Step::Env]).await;
        socket.send(Message::binary(vec![9u8, 9])).await.unwrap();
        let unheard = Watch::Ask {
            id: 3,
            frame: after,
        };
        let _ = socket.send(Message::binary(abi::encode(&unheard))).await;
        let mut heard = Vec::new();
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Binary(bytes) = &message {
                let pushed = abi::decode::<Pushed>(bytes);
                panic!("the socket still answered after a frame that is no Watch: {pushed:?}");
            }
            heard.push(message);
        }
        heard.retain(|message| !matches!(message, Message::Ping(_)));
        assert!(
            matches!(heard.as_slice(), [Message::Close(Some(_))]),
            "{heard:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}
