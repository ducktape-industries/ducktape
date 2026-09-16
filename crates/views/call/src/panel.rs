//! Call window presentation; device effects remain in the active session.
use ducktape_view_guest::{kit, slots, wire};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Panel {
    pub instance: u64,
    pub channel: String,
    pub joined: bool,
    pub loading: bool,
    pub dark: bool,
    pub network: String,
    pub endpoint: String,
    pub account: String,
    pub user_key: String,
    pub status: String,
    pub joined_at: i64,
    pub now: i64,
    pub muted: bool,
    pub camera: bool,
    pub sharing: bool,
    pub speaking: bool,
    pub stage: String,
    pub tiles: Vec<String>,
    pub video_live: bool,
    pub peers: Vec<Peer>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Room {
    pub title: String,
    pub members: Vec<Member>,
    pub roster: Vec<Person>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RoomKey {
    pub instance: u64,
    channel: String,
    network: String,
    endpoint: String,
    account: String,
    user_key: String,
}

pub fn rooms(key: RoomKey) -> ducktape_view_guest::Subscription<crate::Message> {
    use futures::{StreamExt as _, stream};
    ducktape_view_guest::Subscription::run_with(key, |key| {
        let key = key.clone();
        let live = stream::select(
            ducktape_view_guest::host::subscribe("rpc.live", b"chat"),
            ducktape_view_guest::host::subscribe("rpc.live", b"identity"),
        );
        stream::once(load(key.clone())).chain(live.then(move |_| load(key.clone())))
    })
}

async fn load(key: RoomKey) -> crate::Message {
    let result = room(&key).await;
    crate::Message::RoomLoaded(key, result)
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Person {
    key: String,
    node: String,
    label: String,
    initials: String,
    is_you: bool,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Peer {
    peer: String,
    muted: bool,
    speaking: bool,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Member {
    pub key: String,
    label: String,
}

#[derive(Clone)]
pub enum Action {
    Mute,
    Camera,
    Screen,
    Channel,
    Leave,
    Invite(String),
}

pub fn notify(kind: &str) {
    ducktape_view_guest::host::notify(kind, b"null");
}

async fn request(kind: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes =
        ducktape_view_guest::host::request(kind, &serde_json::to_vec(&payload).expect("query"))
            .await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn room(key: &RoomKey) -> Result<Room, String> {
    let channel = &key.channel;
    use serde_json::json;
    let mut names = std::collections::BTreeMap::new();
    let mut accounts_by_key = std::collections::BTreeMap::new();
    let mut from = 0u64;
    loop {
        let Ok(reply) = request(
            "rpc.query",
            json!({"target":"identity", "query":{"all":{"from":from,"limit":128}}}),
        )
        .await
        else {
            break;
        };
        let Some(accounts) = reply["accounts"].as_array() else {
            break;
        };
        if accounts.is_empty() {
            break;
        }
        for account in accounts {
            let Some(number) = account["number"].as_u64() else {
                continue;
            };
            let label = account["name"].as_str().unwrap_or_default();
            names.insert(format!("acct:{number}"), label.to_owned());
            for key in account["keys"].as_array().into_iter().flatten() {
                let bytes: Option<Vec<u8>> = key["pubkey"].as_array().and_then(|bytes| {
                    bytes
                        .iter()
                        .map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                        .collect()
                });
                if let Some(bytes) = bytes {
                    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                    accounts_by_key.insert(key.clone(), format!("acct:{number}"));
                    names.insert(key, label.to_owned());
                }
            }
        }
        let next = accounts
            .last()
            .and_then(|account| account["number"].as_u64())
            .and_then(|number| number.checked_add(1))
            .ok_or("invalid identity cursor")?;
        if next <= from {
            return Err("identity cursor did not advance".into());
        }
        from = next;
    }
    let canonical = |party: &str| {
        let raw = party.strip_prefix("user:").unwrap_or(party);
        accounts_by_key
            .get(raw)
            .cloned()
            .unwrap_or_else(|| raw.to_owned())
    };
    let me = if key.account.is_empty() {
        canonical(&key.user_key)
    } else {
        format!("acct:{}", key.account)
    };
    let reply = request(
        "rpc.view",
        json!({"target":"chat", "query":{"channel":{"channel_id":channel}}}),
    )
    .await?;
    let info = &reply["channel"];
    let title = info["name"].as_str().ok_or("missing call room")?.to_owned();
    let roster = info["huddle"]
        .as_array()
        .ok_or("missing huddle roster")?
        .iter()
        .map(|seat| {
            let party = seat["party"].as_str().ok_or("missing huddle identity")?;
            let identity = canonical(party);
            let label = names
                .get(&identity)
                .cloned()
                .unwrap_or_else(|| identity.clone());
            let initials = label
                .split_whitespace()
                .filter_map(|word| word.chars().next())
                .take(2)
                .collect::<String>()
                .to_uppercase();
            Ok(Person {
                key: identity.clone(),
                node: seat["node"]
                    .as_str()
                    .ok_or("missing huddle node")?
                    .to_owned(),
                label,
                initials,
                is_you: identity == me,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut members = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let reply = request("rpc.view", json!({"target":"chat", "query":{"members":{"channel_id":channel,"after":after,"limit":128}}})).await?;
        let page = &reply["members"];
        let rows = page["members"].as_array().ok_or("missing members page")?;
        for row in rows {
            let party = row["party"].as_str().ok_or("missing member identity")?;
            let key = canonical(party);
            let label = names.get(&key).cloned().unwrap_or_else(|| key.clone());
            members.push(Member { key, label });
        }
        if page["has_more"].as_bool() != Some(true) {
            return Ok(Room {
                title,
                members,
                roster,
            });
        }
        let next = page["next_after"]
            .as_str()
            .ok_or("missing members cursor")?;
        let advances = after.as_deref().is_none_or(|previous| next > previous);
        if !advances {
            return Err("members cursor did not advance".into());
        }
        after = Some(next.to_owned());
    }
}

pub async fn invite(channel: String, room: String, key: String) -> Result<(), String> {
    use ducktape_view_composer::{Send, host as composer};
    let mention = match key.strip_prefix("acct:") {
        Some(account) => format!("<@{account}>"),
        None => format!("<@key:{key}>"),
    };
    let id = composer::id().await?;
    composer::submit(
        id,
        &Send {
            body: format!("{mention} come join the huddle in #{room}"),
            attachments: Vec::new(),
        },
        &composer::Target::Post {
            channel,
            thread: None,
        },
    )
    .await
}

fn button(key: &str, label: &str, action: Action, preset: wire::ButtonPreset) -> wire::Node {
    kit::button(
        key,
        label,
        Some(slots::message(crate::Message::Action(action))),
        preset,
    )
}

impl Panel {
    pub fn room_key(&self) -> RoomKey {
        RoomKey {
            instance: self.instance,
            channel: self.channel.clone(),
            network: self.network.clone(),
            endpoint: self.endpoint.clone(),
            account: self.account.clone(),
            user_key: self.user_key.clone(),
        }
    }

    fn video(&self) -> wire::Node {
        use wire::Length;
        let tiles: Vec<_> = self
            .tiles
            .iter()
            .enumerate()
            .map(|(index, resource)| {
                let mut tile = kit::image_resource(format!("huddle/tile/{index}"), resource);
                if let wire::Node::Image { fit, .. } = &mut tile {
                    *fit = Some(wire::ContentFit::Cover);
                }
                tile
            })
            .collect();
        if !self.stage.is_empty() {
            let strip = tiles
                .into_iter()
                .map(|tile| kit::sized(tile, Some(Length::Fixed(128.)), Some(Length::Fixed(96.))));
            return kit::sized(
                kit::row("huddle/tiles", strip),
                Some(Length::Fill),
                Some(Length::Fixed(96.)),
            );
        }
        let (_, columns) = grid_shape(tiles.len());
        let rows = tiles
            .chunks(columns.max(1))
            .enumerate()
            .map(|(index, tiles)| {
                kit::sized(
                    kit::row(format!("huddle/tiles/row/{index}"), tiles.iter().cloned()),
                    Some(Length::Fill),
                    Some(Length::Fill),
                )
            });
        kit::sized(
            kit::column("huddle/tiles", rows),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }

    pub fn view(
        &self,
        room: &Room,
        invited: &std::collections::BTreeSet<String>,
        error: &str,
    ) -> wire::Node {
        use wire::{ButtonPreset as Style, Length};
        kit::set_dark(self.dark);
        let elapsed = if self.joined_at > 0 {
            let seconds = (self.now - self.joined_at).max(0);
            format!("{}:{:02}", seconds / 60, seconds % 60)
        } else {
            String::new()
        };
        let header = kit::centered_row(
            "huddle/header",
            [
                kit::strong("huddle/title", &room.title),
                kit::caption("huddle/elapsed", elapsed),
                kit::secondary("huddle/status", &self.status),
            ],
        );
        let mut body = Vec::new();
        if !self.stage.is_empty() {
            body.push(kit::image_resource("huddle/stage", &self.stage));
        }
        if self.video_live {
            body.push(self.video());
        }
        body.push(kit::caption(
            "huddle/count",
            format!("In the huddle · {}", room.roster.len()),
        ));
        let rows = room.roster.iter().map(|person| {
            let peer = self.peers.iter().find(|peer| peer.peer == person.node);
            let (muted, speaking) = if person.is_you {
                (self.muted, self.speaking)
            } else {
                (
                    peer.is_some_and(|peer| peer.muted),
                    peer.is_some_and(|peer| peer.speaking && !peer.muted),
                )
            };
            let caption = match (person.is_you, muted) {
                (true, true) => "you · muted",
                (true, false) => "you",
                (false, true) => "muted",
                (false, false) => "",
            };
            let key = format!("huddle/person/{}", person.node);
            let color = if speaking {
                kit::palette().success
            } else {
                kit::palette().muted
            };
            kit::centered_row(
                &key,
                [
                    kit::colored(
                        kit::strong(format!("{key}/avatar"), &person.initials),
                        color,
                    ),
                    kit::text(format!("{key}/label"), &person.label),
                    kit::caption(format!("{key}/caption"), caption),
                ],
            )
        });
        let height = if self.video_live {
            Length::Fixed(150.)
        } else {
            Length::Fill
        };
        body.push(kit::sized(
            kit::scroll("huddle/roster-scroll", kit::column("huddle/roster", rows)),
            Some(Length::Fill),
            Some(height),
        ));
        let invites = room
            .members
            .iter()
            .filter(|member| {
                !invited.contains(&member.key)
                    && !room.roster.iter().any(|person| person.key == member.key)
            })
            .map(|member| {
                button(
                    &format!("huddle/invite/{}", member.key),
                    &member.label,
                    Action::Invite(member.key.clone()),
                    Style::Secondary,
                )
            })
            .collect::<Vec<_>>();
        if !invites.is_empty() {
            body.push(kit::caption("huddle/invite-title", "Invite"));
            body.push(kit::wrapped_row("huddle/invites", invites));
        }
        if !error.is_empty() {
            body.push(kit::colored(
                kit::text("huddle/error", error),
                kit::palette().danger,
            ));
        }
        let controls = kit::wrapped_row(
            "huddle/controls",
            [
                button(
                    "huddle/mute",
                    if self.muted { "Unmute" } else { "Mute" },
                    Action::Mute,
                    if self.muted {
                        Style::Danger
                    } else {
                        Style::Secondary
                    },
                ),
                button(
                    "huddle/camera",
                    if self.camera { "Stop camera" } else { "Camera" },
                    Action::Camera,
                    if self.camera {
                        Style::Primary
                    } else {
                        Style::Secondary
                    },
                ),
                button(
                    "huddle/screen",
                    if self.sharing {
                        "Stop sharing"
                    } else {
                        "Share screen"
                    },
                    Action::Screen,
                    if self.sharing {
                        Style::Primary
                    } else {
                        Style::Secondary
                    },
                ),
                button(
                    "huddle/channel",
                    "Go to channel",
                    Action::Channel,
                    Style::Text,
                ),
                button("huddle/leave", "Leave huddle", Action::Leave, Style::Danger),
            ],
        );
        kit::sized(
            kit::column(
                "huddle",
                [
                    header,
                    kit::sized(
                        kit::column("huddle/body", body),
                        Some(Length::Fill),
                        Some(Length::Fill),
                    ),
                    controls,
                ],
            ),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }
}

/// The grid a stage-less huddle lays its tiles in: as square as the count
/// allows (1, 2 side by side, 2×2, 2×3, 3×3 …), rows filled left to right.
fn grid_shape(tiles: usize) -> (usize, usize) {
    if tiles == 0 {
        return (0, 0);
    }
    let rows = (tiles as f32).sqrt().floor() as usize;
    let cols = tiles.div_ceil(rows);
    (rows, cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_grid_is_as_square_as_the_count_allows() {
        assert_eq!(grid_shape(0), (0, 0));
        assert_eq!(grid_shape(1), (1, 1));
        assert_eq!(grid_shape(2), (1, 2));
        assert_eq!(grid_shape(3), (1, 3));
        assert_eq!(grid_shape(4), (2, 2));
        assert_eq!(grid_shape(5), (2, 3));
        assert_eq!(grid_shape(9), (3, 3));
        assert_eq!(grid_shape(10), (3, 4));
    }

    #[test]
    fn panel_owns_status_invites_and_resource_layout() {
        let panel: Panel = serde_json::from_value(serde_json::json!({
            "title":"Engineering", "muted":true, "video_live":true,
            "stage":"image:stage", "tiles":["image:tile"],
            "roster":[{"key":"user:aa", "node":"node-a", "label":"Alice", "is_you":true}],
            "invitees":[{"key":"aa", "label":"Alice"}, {"key":"bb", "label":"Bob"}]
        }))
        .unwrap();
        let members = vec![
            Member {
                key: "aa".into(),
                label: "Alice".into(),
            },
            Member {
                key: "bb".into(),
                label: "Bob".into(),
            },
        ];
        let room = Room {
            title: "Engineering".into(),
            members,
            roster: vec![Person {
                key: "aa".into(),
                node: "node-a".into(),
                label: "Alice".into(),
                is_you: true,
                ..Default::default()
            }],
        };
        let mut tree = panel.view(&room, &Default::default(), "");
        let mut text = Vec::new();
        let mut resources = Vec::new();
        let mut invites = Vec::new();
        tree.for_each_mut(&mut |node| match node {
            wire::Node::Text { content, .. } => text.push(content.clone()),
            wire::Node::Image {
                data: Some(wire::ImageData::Resource(key)),
                ..
            } => resources.push(key.clone()),
            wire::Node::Button { key, .. } if key.starts_with("huddle/invite/") => {
                invites.push(key.clone())
            }
            _ => {}
        });
        assert!(text.iter().any(|text| text == "you · muted"));
        assert_eq!(resources, ["image:stage", "image:tile"]);
        assert_eq!(invites, ["huddle/invite/bb"]);
    }
}
