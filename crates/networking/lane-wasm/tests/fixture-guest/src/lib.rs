//! Echo guest: each event returns the effect that mirrors it. A `tick` at
//! `now-ms == u64::MAX` loops forever, which is how the envelope tests trip
//! the fuel budget.

wit_bindgen::generate!({
    world: "media",
    path: "../../wit",
});

use ducktape::lane::host::{self, Level};
use ducktape::lane::types::{ClientSend, Close, CloseFlow, LaneSend, LogLine, OpenFlow, SetRoster};

struct Echo;

impl Guest for Echo {
    fn init(config: Config) -> Result<(), String> {
        if config.lanes.is_empty() {
            return Err("no lanes".into());
        }
        host::log(
            Level::Info,
            "echo",
            &format!("init with {} lanes", config.lanes.len()),
        );
        Ok(())
    }

    fn step(event: Event, now_ms: u64) -> Vec<Effect> {
        match event {
            Event::Tick => {
                while now_ms == u64::MAX {
                    core::hint::black_box(());
                }
                Vec::new()
            }
            Event::Datagram(d) => vec![Effect::LaneSend(LaneSend {
                lane: d.lane,
                peer: d.peer,
                bytes: d.bytes,
            })],
            Event::ClientFrame(f) => vec![Effect::ClientSend(ClientSend {
                session: f.session,
                frame: f.frame,
            })],
            Event::Roster(r) => vec![Effect::SetRoster(SetRoster {
                lane: 2,
                flow: r.session,
                peers: r.peers,
            })],
            Event::SessionOpened(s) => vec![
                Effect::OpenFlow(OpenFlow {
                    lane: 2,
                    flow: s.session,
                    max_queued: 128,
                }),
                Effect::Log(LogLine {
                    level: Level::Info,
                    message: s.channel,
                }),
            ],
            Event::SessionClosed(session) => vec![
                Effect::CloseFlow(CloseFlow {
                    lane: 2,
                    flow: session,
                }),
                Effect::Close(Close {
                    session,
                    reason: "closed".into(),
                }),
            ],
        }
    }
}

export!(Echo);
