//! Device files granted by an OS picker or clipboard gesture, scoped to one guest.
use super::kernel::spawn_device as spawn;
use super::{Guest, NativeModuleView};
use gpui_kit::{ClipboardEntry, Context};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const MAX_CHUNK: usize = 256 << 10;
const MAX_HANDLES: usize = 128;

#[derive(serde::Serialize)]
struct FileInfo {
    token: String,
    name: String,
    bytes: u64,
}

enum Content {
    Disk(Mutex<File>),
    Bytes(Arc<[u8]>),
}
#[derive(Default)]
enum State {
    #[default]
    Retired,
    Active {
        files: HashMap<String, Arc<Content>>,
    },
}

pub(super) struct Filesystem {
    state: Arc<Mutex<State>>,
    pending: Vec<(u64, DeviceRequest)>,
    drops: Option<u64>,
}
enum DeviceRequest {
    Pick,
    ClipboardRead,
    ClipboardWrite(String),
}
impl Default for Filesystem {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Active {
                files: HashMap::new(),
            })),
            pending: Vec::new(),
            drops: None,
        }
    }
}
impl Drop for Filesystem {
    fn drop(&mut self) {
        *self.state.lock().unwrap() = State::Retired;
    }
}

fn grant(
    state: &Mutex<State>,
    name: String,
    content: Content,
    bytes: u64,
) -> Result<FileInfo, String> {
    let mut state = state.lock().unwrap();
    let State::Active { files } = &mut *state else {
        return Err("file grant belongs to a retired guest".into());
    };
    if files.len() >= MAX_HANDLES {
        return Err("too many open file grants".into());
    }
    // Random identities cannot alias a serialized stale grant after another
    // guest or a restarted process picks its first file.
    let token = format!("{:032x}", rand::random::<u128>());
    let collides = files.contains_key(&token);
    if collides {
        return Err("file grant identity collision".into());
    }
    files.insert(token.clone(), Arc::new(content));
    Ok(FileInfo { token, name, bytes })
}
fn grant_path(state: &Mutex<State>, path: PathBuf) -> Result<FileInfo, String> {
    let file = File::open(&path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("the selection is not a file".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("file name is not UTF-8")?
        .to_owned();
    grant(state, name, Content::Disk(Mutex::new(file)), metadata.len())
}
fn content(state: &Mutex<State>, token: &str) -> Result<Arc<Content>, String> {
    let state = state.lock().unwrap();
    let State::Active { files, .. } = &*state else {
        return Err("file grant belongs to a retired guest".into());
    };
    files
        .get(token)
        .cloned()
        .ok_or_else(|| "unknown file grant".into())
}
fn read_chunk(file: &Content, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 || len > MAX_CHUNK {
        return Err("file read exceeds chunk bounds".into());
    }
    match file {
        Content::Disk(file) => {
            let mut file = file.lock().unwrap();
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let mut bytes = vec![0; len];
            let count = file.read(&mut bytes).map_err(|error| error.to_string())?;
            bytes.truncate(count);
            Ok(bytes)
        }
        Content::Bytes(bytes) => {
            let offset = usize::try_from(offset).map_err(|_| "file offset out of range")?;
            let end = offset.saturating_add(len).min(bytes.len());
            Ok(bytes.get(offset..end).unwrap_or_default().to_vec())
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    token: String,
    offset: u64,
    len: usize,
}

impl Filesystem {
    pub(super) fn cancel(&mut self, id: u64) {
        self.pending.retain(|(pending, _)| *pending != id);
        if self.drops == Some(id) {
            self.drops = None;
        }
    }
}

pub(super) fn answer(
    guest: &mut Guest,
    capability: &str,
    operation: &str,
    id: u64,
    payload: &[u8],
) -> bool {
    match (capability, operation) {
        ("fs", "drops") => {
            if let Some(previous) = guest.filesystem.drops.replace(id) {
                guest.reply(previous, Ok(Vec::new()));
            }
        }
        ("fs", "pick") => device(guest, id, DeviceRequest::Pick),
        ("clipboard", "read") => device(guest, id, DeviceRequest::ClipboardRead),
        ("clipboard", "write") => {
            let text = match String::from_utf8(payload.to_vec()) {
                Ok(text) => text,
                Err(_) => {
                    guest.refuse(id, "clipboard text is not UTF-8".into());
                    return true;
                }
            };
            device(guest, id, DeviceRequest::ClipboardWrite(text));
        }
        ("fs", "read") => {
            let request = serde_json::from_slice::<ReadRequest>(payload)
                .map_err(|error| error.to_string())
                .and_then(|request| {
                    let file = content(&guest.filesystem.state, &request.token)?;
                    if request.len == 0 || request.len > MAX_CHUNK {
                        return Err("file read exceeds chunk bounds".into());
                    }
                    Ok((file, request))
                });
            match request {
                Ok((file, request)) => spawn(guest, id, async move {
                    tokio::task::spawn_blocking(move || {
                        read_chunk(&file, request.offset, request.len)
                    })
                    .await
                    .map_err(|error| error.to_string())?
                }),
                Err(error) => guest.refuse(id, error),
            }
        }
        ("fs", "release") => {
            let token = std::str::from_utf8(payload).unwrap_or_default();
            if let State::Active { files, .. } = &mut *guest.filesystem.state.lock().unwrap() {
                files.remove(token);
            }
            guest.reply(id, Ok(Vec::new()));
        }
        _ => return false,
    }
    true
}

// Called only by the native input route after its guest identity checks. Wire
// observations delivered by a guest cannot mint authority over an OS path.
pub(super) fn observe_drop(guest: &mut Guest, event: &super::wire::Event) -> bool {
    let super::wire::Event::Observation {
        event: super::wire::events::Event::Window(super::wire::events::Window::FileDropped(path)),
        ..
    } = event
    else {
        return false;
    };
    let Some(id) = guest.filesystem.drops else {
        return false;
    };
    let result = grant_path(&guest.filesystem.state, PathBuf::from(path))
        .and_then(|file| serde_json::to_vec(&vec![file]).map_err(|error| error.to_string()));
    guest.pending.push(super::wire::Event::Response {
        id,
        result,
        done: false,
    });
    true
}

fn device(guest: &mut Guest, id: u64, request: DeviceRequest) {
    if guest.filesystem.pending.len() >= 16 {
        guest.refuse(id, "too many pending device requests".into());
        return;
    }
    guest.filesystem.pending.push((id, request));
}

pub(super) fn mount(guest: &mut Guest, cx: &mut Context<NativeModuleView>) {
    for (id, request) in std::mem::take(&mut guest.filesystem.pending) {
        match request {
            DeviceRequest::Pick => {
                let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: true,
                    prompt: Some("Choose files".into()),
                });
                let state = guest.filesystem.state.clone();
                spawn(guest, id, async move {
                    let paths = chosen
                        .await
                        .map_err(|error| error.to_string())?
                        .map_err(|error| error.to_string())?
                        .unwrap_or_default();
                    let files = paths
                        .into_iter()
                        .map(|path| grant_path(&state, path))
                        .collect::<Result<Vec<_>, _>>()?;
                    serde_json::to_vec(&files).map_err(|error| error.to_string())
                });
            }
            DeviceRequest::ClipboardRead => {
                let mut text = String::new();
                let mut files = Vec::new();
                let mut failure = None;
                if let Some(item) = cx.read_from_clipboard() {
                    for entry in item.entries() {
                        let granted = match entry {
                            ClipboardEntry::String(value) => {
                                text.push_str(value.text());
                                continue;
                            }
                            ClipboardEntry::ExternalPaths(paths) => paths
                                .paths()
                                .iter()
                                .cloned()
                                .map(|path| grant_path(&guest.filesystem.state, path))
                                .collect::<Result<Vec<_>, _>>(),
                            ClipboardEntry::Image(image) => {
                                let bytes = image.bytes();
                                grant(
                                    &guest.filesystem.state,
                                    format!("pasted.{}", image.format().extension()),
                                    Content::Bytes(bytes.to_vec().into()),
                                    bytes.len() as u64,
                                )
                                .map(|file| vec![file])
                            }
                        };
                        match granted {
                            Ok(granted) => files.extend(granted),
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                }
                let result = match failure {
                    Some(error) => Err(error),
                    None => serde_json::to_vec(&serde_json::json!({"text":text,"files":files}))
                        .map_err(|error| error.to_string()),
                };
                guest.reply(id, result);
            }
            DeviceRequest::ClipboardWrite(text) => {
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                guest.reply(id, Ok(Vec::new()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_grants_are_scoped_bounded_and_revoked_on_retirement() {
        let owner = Filesystem::default();
        let another = Filesystem::default();
        let file = grant(
            &owner.state,
            "note".into(),
            Content::Bytes(Arc::from(&b"abcdef"[..])),
            6,
        )
        .unwrap();
        assert!(content(&another.state, &file.token).is_err());
        assert!(content(&owner.state, "/etc/passwd").is_err());
        let granted = content(&owner.state, &file.token).unwrap();
        assert_eq!(read_chunk(&granted, 2, 3).unwrap(), b"cde");
        assert!(read_chunk(&granted, 0, MAX_CHUNK + 1).is_err());
        let retired = owner.state.clone();
        drop(owner);
        assert!(content(&retired, &file.token).is_err());
        assert!(
            grant(
                &retired,
                "late".into(),
                Content::Bytes(Arc::from(&b"x"[..])),
                1
            )
            .is_err()
        );
    }
    #[test]
    fn cancelling_a_drop_subscription_revokes_only_its_own_delivery() {
        let mut filesystem = Filesystem::default();
        filesystem.drops = Some(41);
        filesystem.cancel(40);
        assert_eq!(filesystem.drops, Some(41));
        filesystem.cancel(41);
        assert_eq!(filesystem.drops, None);
    }
    #[test]
    fn reopened_guest_grants_do_not_alias_serialized_tokens() {
        let first = Filesystem::default();
        let old = grant(
            &first.state,
            "a".into(),
            Content::Bytes(Arc::from(&b"a"[..])),
            1,
        )
        .unwrap();
        drop(first);
        let next = Filesystem::default();
        let new = grant(
            &next.state,
            "b".into(),
            Content::Bytes(Arc::from(&b"b"[..])),
            1,
        )
        .unwrap();
        assert_ne!(old.token, new.token);
        assert!(content(&next.state, &old.token).is_err());
    }
    #[test]
    fn read_requests_cannot_substitute_paths_for_grants() {
        assert!(
            serde_json::from_str::<ReadRequest>(
                r#"{"token":"1","offset":0,"len":1,"path":"/etc/passwd"}"#
            )
            .is_err()
        );
    }
}
