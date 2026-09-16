//! THE LIVE HUDDLE LANE — ignored by default, because it needs a live node, a
//! second person, and the camera and microphone this box may not have.
//!
//! The deployed call guest drives this side's native camera and microphone
//! through the generic media resources. The independently installed media
//! service must be published for the room owner's account, and the registry
//! must deploy the `call` view. The process and guest unit suites cover
//! authentication, protocol, and lifecycle without physical devices.
//!
//! ONE PROCESS IS ONE PERSON, which is the whole reason this is not a
//! two-session test: identity is process-global (`DUCKTAPE_HOME`/
//! `DUCKTAPE_USER_KEY` name ONE user key), and it is the roster row matching
//! that key that a session filters itself out by. Two sessions in one process
//! are one person twice, which is not the arrangement that broke.
//!
//! IT ASKS FOR EVERYTHING A HUDDLE IS: the other side's presence beacon,
//! their picture in the store, and mixed frames with their voice in them. Two
//! of the three passing is the exact half-working call this whole session was
//! about — so one assertion covers all three, and the failure names which.
//!
//! `ops/huddle-lane.sh` stands the network up and prints the two commands.
//! Each side runs:
//!
//! ```text
//! DUCKTAPE_HOME=<side dir> DUCKTAPE_NODE=http://127.0.0.1:<http port> \
//! DUCKTAPE_HUDDLE_PASSWORD=<key password> DUCKTAPE_HUDDLE_CHANNEL=eng \
//! cargo test -p ducktape-app -- --ignored --nocapture huddle_live
//! ```
//!
//! `DUCKTAPE_HUDDLE_SOURCE=screen` publishes this side's DESKTOP instead of
//! its camera — the same one-flow, one-tile arrangement the share button
//! makes, so the far side names what it received by its size: a camera is
//! 640×480, a desktop is whatever that root window is, halved onto the tile
//! budget. It needs a real X display (`DISPLAY=:99` under Xvfb is one).
//!
//! It also prints how THIS side's own picture arrived — frames, mean gap,
//! worst gap. A preview averaging 30 fps with one 200 ms hole in it is the
//! stutter, and only the worst gap can see the hole.

use std::time::Duration;

use futures::StreamExt as _;

/// The huddle is a meeting: this side waits for the other one to show up,
/// join, and start publishing. Generous on purpose — a person is slower than
/// a test.
const MEETING: Duration = Duration::from_secs(120);
/// How long this side keeps publishing after it is satisfied, so the other
/// side — a second behind at worst — still has somebody to see.
const COURTESY: Duration = Duration::from_secs(15);

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be set — see this module's doc, or ops/huddle-lane.sh")
    })
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs a live node, a second side, and a camera; see the module doc"]
async fn this_side_hears_and_sees_the_other_through_the_apps_own_leg() {
    let node = required("DUCKTAPE_NODE");
    let password = required("DUCKTAPE_HUDDLE_PASSWORD");
    let channel = std::env::var("DUCKTAPE_HUDDLE_CHANNEL").unwrap_or_else(|_| "eng".into());

    // Join the way the button joins: this device's user key signs, and the
    // node key it stamps is the one its own `/v1/status` publishes.
    crate::backend::join_huddle(node.clone(), password, channel.clone())
        .await
        .expect("this side joins the huddle");

    let client = crate::backend::rpc_client(&node).expect("live node client");
    crate::module_view::connected(&client).settled().await;
    let joined_at = std::time::Instant::now();
    let mut events = crate::call::call_session(node, channel);

    // What this side publishes. Both are one video flow and one tile at the
    // far end, so a screen is not a second stream — it is the other source.
    // A box with no such device turns the toggle back off and says why on the
    // status line, which is what the failure below prints.
    let sharing = std::env::var("DUCKTAPE_HUDDLE_SOURCE").is_ok_and(|source| source == "screen");
    match sharing {
        true => assert!(
            crate::video::call_use_screen(true).sharing,
            "the share toggle must take"
        ),
        false => assert!(
            crate::video::call_use_camera(true).camera,
            "the camera toggle must take"
        ),
    }

    let mut seen_peer = false;
    let mut peer = String::new();
    let mut note = String::new();
    let mut peer_image = String::new();
    let watch = async {
        loop {
            // Every event is also a chance to ask the store whether their
            // picture landed: frames arrive on a blocking decode task, not on
            // this stream, and the 1 Hz beacons keep this loop turning.
            let Some(event) = events.next().await else {
                panic!("the session ended before the huddle worked ({note})");
            };
            if !event.message.is_empty() {
                note = format!("{}: {}", event.kind, event.message);
            }
            if let [other] = event.peers.as_slice() {
                seen_peer = true;
                peer.clone_from(&other.peer);
                peer_image.clone_from(&other.image);
            }
            if !seen_peer {
                continue;
            }
            let Some((width, height, _)) = crate::video::stage_frame(&peer_image) else {
                continue;
            };
            let heard = crate::call::voice_frames_heard();
            if heard == 0 {
                continue;
            }
            // Program output, not logging: the lane is run by hand and the
            // numbers ARE the result — a picture off the far camera (640×480)
            // or their desktop (whatever their root window is, halved onto the
            // tile budget), and mixed frames with sound in them off the far
            // microphone. Neither comes out of an empty store.
            println!(
                "peer {peer} is here: picture {width}x{height}, {heard} audible frames, \
                 {:?} after this side joined",
                joined_at.elapsed()
            );
            return;
        }
    };
    tokio::time::timeout(MEETING, watch)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the other side must arrive, be seen AND be heard — beacon: {seen_peer}, \
                 picture: {}, audible frames: {} (last note: {note})",
                crate::video::stage_frame(&peer_image).is_some(),
                crate::call::voice_frames_heard(),
            )
        });

    // STAY IN THE ROOM. Leaving the instant this side is satisfied takes this
    // camera with it, and the other side — which may be a second behind — then
    // waits for a picture nobody is sending any more. A real participant does
    // not hang up the moment they can see you.
    let _ = tokio::time::timeout(COURTESY, async { while events.next().await.is_some() {} }).await;

    // HOW THE SELF-VIEW ARRIVED, which is the other half of "usable": a
    // preview that averages 30 fps with one 200 ms hole in it is a stutter,
    // and only the worst gap can see the hole. A camera's own cadence sets the
    // floor (30 fps ≈ 33.3 ms); a shared screen is paced by the capture loop.
    let pace = crate::video::preview_pace();
    let mean_us = pace.total_gap_us / pace.frames.saturating_sub(1).max(1);
    println!(
        "this side's own picture: {} frames, mean gap {:.1} ms, worst gap {:.1} ms",
        pace.frames,
        mean_us as f64 / 1000.0,
        pace.worst_gap_us as f64 / 1000.0,
    );
}
