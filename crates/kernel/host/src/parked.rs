//! The kernel's record of parked frames, in `$parked`: what each frame runs as and for whom, who
//! waits on it, its packed memory image as a blob held on the frame's line, and the counters.
use abi::role::awaits::{FrameId, Ticket};
use abi::{BlobId, Cause, HashKind, ItemRef, Origin, Principal, ProgramId, Scan};
use blob::{Layered, Stage};
use borsh::{BorshDeserialize, BorshSerialize};
use runtime::{Image, Packed, Page};
use state::{Overlay, Reads, View};

use crate::namespace::{self, BLOBS, PARKED};
use crate::{Result, corrupt};

const KIND: &str = "parked";

const NEXT_FRAME: &[u8] = b"next/frame";
const NEXT_TICKET: &[u8] = b"next/ticket";

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Waiter {
    Nobody,
    Reply { emitter: ProgramId, item: ItemRef },
    Call { ticket: Ticket },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Parked {
    pub program: ProgramId,
    pub code: BlobId,
    pub origin: Origin,
    pub sender: Option<Principal>,
    pub cause: Cause,
    pub waiter: Waiter,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Shape {
    length: u64,
    pages: Vec<u32>,
}

fn frame_key(id: FrameId) -> Vec<u8> {
    [b"frame/".as_slice(), &id.to_be_bytes()].concat()
}

fn shape_key(id: FrameId) -> Vec<u8> {
    [b"shape/".as_slice(), &id.to_be_bytes()].concat()
}

fn image_key(id: FrameId) -> Vec<u8> {
    [b"image/".as_slice(), &id.to_be_bytes()].concat()
}

fn blob_key(id: FrameId) -> Vec<u8> {
    [b"blob/".as_slice(), &id.to_be_bytes()].concat()
}

fn page_key(id: FrameId, index: u32) -> Vec<u8> {
    [b"page/".as_slice(), &id.to_be_bytes(), &index.to_be_bytes()].concat()
}

fn client_key(id: FrameId) -> Vec<u8> {
    [b"client/".as_slice(), &id.to_be_bytes()].concat()
}

fn of_program(program: &str) -> Vec<u8> {
    [b"of/".as_slice(), program.as_bytes(), b"/"].concat()
}

fn of_key(program: &str, id: FrameId) -> Vec<u8> {
    [of_program(program), id.to_be_bytes().to_vec()].concat()
}

pub fn counter(view: &View<'_>, space: &str, key: &[u8]) -> Result<u64> {
    let value = view.get(space, key)?;
    Ok(value
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0))
}

fn take(storage: &Reads, overlay: &mut Overlay, key: &[u8]) -> Result<u64> {
    let next = counter(&View::new(storage, vec![&*overlay]), PARKED, key)?;
    overlay.set(PARKED, key.to_vec(), (next + 1).to_be_bytes().to_vec());
    Ok(next)
}

pub fn take_ticket(storage: &Reads, overlay: &mut Overlay) -> Result<Ticket> {
    take(storage, overlay, NEXT_TICKET)
}

pub fn take_frame(storage: &Reads, overlay: &mut Overlay) -> Result<FrameId> {
    take(storage, overlay, NEXT_FRAME)
}

pub fn record(view: &View<'_>, id: FrameId) -> Result<Option<Parked>> {
    let Some(bytes) = view.get(PARKED, &frame_key(id))? else {
        return Ok(None);
    };
    abi::decode(&bytes).map(Some).map_err(corrupt)
}

fn unpacked(id: FrameId, bytes: &[u8]) -> Result<Image> {
    let packed: Packed = abi::decode(bytes).map_err(corrupt)?;
    packed
        .unpack()
        .map_err(|error| crate::Error::Corrupt(format!("frame {id}'s image: {error}")))
}

pub fn load(view: &View<'_>, blobs: &Layered<'_>, id: FrameId) -> Result<Option<(Parked, Image)>> {
    let Some(parked) = record(view, id)? else {
        return Ok(None);
    };
    if let Some(bytes) = view.get(PARKED, &blob_key(id))? {
        let held: BlobId = abi::decode(&bytes).map_err(corrupt)?;
        let blob = blobs
            .get(&held)?
            .ok_or(crate::Error::BlobUnavailable(held))?;
        return Ok(Some((parked, unpacked(id, &blob.body)?)));
    }
    if let Some(bytes) = view.get(PARKED, &image_key(id))? {
        return Ok(Some((parked, unpacked(id, &bytes)?)));
    }
    let shape = view
        .get(PARKED, &shape_key(id))?
        .ok_or_else(|| crate::Error::Corrupt(format!("frame {id} has no image")))?;
    let shape: Shape = abi::decode(&shape).map_err(corrupt)?;
    let mut pages = Vec::with_capacity(shape.pages.len());
    for index in shape.pages {
        let bytes = view
            .get(PARKED, &page_key(id, index))?
            .ok_or_else(|| crate::Error::Corrupt(format!("frame {id} lost page {index}")))?;
        pages.push(Page { index, bytes });
    }
    let image = Image {
        length: shape.length,
        pages,
    };
    Ok(Some((parked, image)))
}

pub fn save(
    storage: &Reads,
    overlay: &mut Overlay,
    stage: &mut Stage,
    id: FrameId,
    parked: Parked,
    image: Image,
    as_blob: bool,
) -> Result<()> {
    drop_image(storage, overlay, id)?;
    let packed = image
        .pack()
        .map_err(|error| crate::Error::Corrupt(format!("frame {id}'s image: {error}")))?;
    let bytes = abi::encode(&packed);
    match as_blob {
        true => hold(storage, overlay, stage, id, &bytes)?,
        false => overlay.set(PARKED, image_key(id), bytes),
    }
    overlay.set(PARKED, of_key(&parked.program, id), Vec::new());
    overlay.set(PARKED, frame_key(id), abi::encode(&parked));
    Ok(())
}

pub fn client(view: &View<'_>, id: FrameId) -> Result<Option<String>> {
    let Some(bytes) = view.get(PARKED, &client_key(id))? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| crate::Error::Corrupt(format!("frame {id}'s client is not UTF-8")))
}

pub fn keep_client(
    storage: &Reads,
    overlay: &mut Overlay,
    id: FrameId,
    client: Option<&str>,
) -> Result<()> {
    match client {
        Some(client) => overlay.set(PARKED, client_key(id), client.as_bytes().to_vec()),
        None => drop_client(storage, overlay, id)?,
    }
    Ok(())
}

fn drop_client(storage: &Reads, overlay: &mut Overlay, id: FrameId) -> Result<()> {
    let view = View::new(storage, vec![&*overlay]);
    if view.get(PARKED, &client_key(id))?.is_some() {
        overlay.delete(PARKED, client_key(id));
    }
    Ok(())
}

pub fn remove(storage: &Reads, overlay: &mut Overlay, id: FrameId, program: &str) -> Result<()> {
    drop_image(storage, overlay, id)?;
    drop_client(storage, overlay, id)?;
    overlay.delete(PARKED, of_key(program, id));
    overlay.delete(PARKED, frame_key(id));
    Ok(())
}

pub fn of(view: &View<'_>, program: &str) -> Result<Vec<FrameId>> {
    let prefix = of_program(program);
    let entries = view.scan(PARKED, &Scan::prefix(&prefix))?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| entry.key[prefix.len()..].try_into().ok())
        .map(u64::from_be_bytes)
        .collect())
}

fn roster(
    storage: &Reads,
    overlay: &mut Overlay,
    held: &BlobId,
    change: namespace::Change,
) -> Result<()> {
    let key = namespace::blob(held);
    let row = View::new(storage, vec![&*overlay]).get(BLOBS, &key)?;
    match namespace::changed(row.as_deref(), PARKED, change)? {
        namespace::Roster::Unchanged => {}
        namespace::Roster::Written(value) => overlay.set(BLOBS, key, value),
        namespace::Roster::Dropped => overlay.delete(BLOBS, key),
    }
    Ok(())
}

fn hold(
    storage: &Reads,
    overlay: &mut Overlay,
    stage: &mut Stage,
    id: FrameId,
    bytes: &[u8],
) -> Result<()> {
    let held = stage.put(HashKind::Sha256, KIND, bytes).map_err(corrupt)?;
    let line = frame_key(id);
    overlay.set(
        BLOBS,
        namespace::line(PARKED, &line),
        namespace::blob(&held),
    );
    overlay.set(BLOBS, namespace::on(&held, PARKED, &line), Vec::new());
    roster(storage, overlay, &held, namespace::Change::List)?;
    overlay.set(PARKED, blob_key(id), abi::encode(&held));
    Ok(())
}

fn let_go(storage: &Reads, overlay: &mut Overlay, id: FrameId) -> Result<()> {
    let line = frame_key(id);
    let key = namespace::line(PARKED, &line);
    let row = View::new(storage, vec![&*overlay]).get(BLOBS, &key)?;
    let Some(held) = namespace::latest(row.as_deref())? else {
        return Ok(());
    };
    overlay.delete(BLOBS, key);
    overlay.delete(BLOBS, namespace::on(&held, PARKED, &line));
    roster(storage, overlay, &held, namespace::Change::Unlist)?;
    overlay.delete(PARKED, blob_key(id));
    Ok(())
}

fn drop_image(storage: &Reads, overlay: &mut Overlay, id: FrameId) -> Result<()> {
    let_go(storage, overlay, id)?;
    let view = View::new(storage, vec![&*overlay]);
    if view.get(PARKED, &image_key(id))?.is_some() {
        overlay.delete(PARKED, image_key(id));
    }
    let view = View::new(storage, vec![&*overlay]);
    let Some(shape) = view.get(PARKED, &shape_key(id))? else {
        return Ok(());
    };
    let shape: Shape = abi::decode(&shape).map_err(corrupt)?;
    for index in shape.pages {
        overlay.delete(PARKED, page_key(id, index));
    }
    overlay.delete(PARKED, shape_key(id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use blob::Blobs;
    use runtime::PAGE_BYTES;
    use state::Storage;

    use super::*;

    fn loaded(view: &View<'_>, id: FrameId) -> Option<(Parked, Image)> {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let stage = Stage::default();
        let layered = Layered {
            stage: &stage,
            blobs: &blobs,
        };
        load(view, &layered, id).unwrap()
    }

    fn parked() -> Parked {
        Parked {
            program: "awaiter".into(),
            code: BlobId::Sha256([1; 32]),
            origin: Origin::System,
            sender: None,
            cause: Cause::Direct,
            waiter: Waiter::Nobody,
        }
    }

    fn image() -> Image {
        Image {
            length: 4 * PAGE_BYTES as u64,
            pages: vec![Page {
                index: 2,
                bytes: vec![7; PAGE_BYTES],
            }],
        }
    }

    #[test]
    fn a_frame_is_saved_as_one_packed_image_loads_and_is_removed_whole() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let mut overlay = Overlay::default();
        let record = parked();
        save(
            &storage,
            &mut overlay,
            &mut Stage::default(),
            7,
            record.clone(),
            image(),
            false,
        )
        .unwrap();
        let view = View::new(&storage, vec![&overlay]);
        let packed = abi::encode(&image().pack().unwrap());
        assert_eq!(view.get(PARKED, &image_key(7)).unwrap(), Some(packed));
        assert_eq!(view.get(PARKED, &shape_key(7)).unwrap(), None);
        assert_eq!(view.get(PARKED, &page_key(7, 2)).unwrap(), None);
        assert_eq!(loaded(&view, 7), Some((record.clone(), image())));
        remove(&storage, &mut overlay, 7, &record.program).unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &image_key(7)).unwrap(), None);
        assert_eq!(loaded(&view, 7), None);
    }

    fn keep_page_by_page(overlay: &mut Overlay, id: FrameId, record: &Parked, image: Image) {
        let shape = Shape {
            length: image.length,
            pages: image.pages.iter().map(|page| page.index).collect(),
        };
        for page in image.pages {
            overlay.set(PARKED, page_key(id, page.index), page.bytes);
        }
        overlay.set(PARKED, shape_key(id), abi::encode(&shape));
        overlay.set(PARKED, of_key(&record.program, id), Vec::new());
        overlay.set(PARKED, frame_key(id), abi::encode(record));
    }

    #[test]
    fn a_frame_kept_page_by_page_still_loads_and_its_next_save_packs_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let mut overlay = Overlay::default();
        let record = parked();
        keep_page_by_page(&mut overlay, 7, &record, image());
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &image_key(7)).unwrap(), None);
        assert_eq!(loaded(&view, 7), Some((record.clone(), image())));
        save(
            &storage,
            &mut overlay,
            &mut Stage::default(),
            7,
            record.clone(),
            image(),
            false,
        )
        .unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &page_key(7, 2)).unwrap(), None);
        assert_eq!(view.get(PARKED, &shape_key(7)).unwrap(), None);
        assert!(view.get(PARKED, &image_key(7)).unwrap().is_some());
        assert_eq!(loaded(&view, 7), Some((record.clone(), image())));
        keep_page_by_page(&mut overlay, 8, &record, image());
        remove(&storage, &mut overlay, 8, &record.program).unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &page_key(8, 2)).unwrap(), None);
        assert_eq!(loaded(&view, 8), None);
    }

    #[test]
    fn a_frame_saved_as_a_blob_keeps_only_its_id_and_its_line_holds_the_blob_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let blobs = Blobs::open(&dir.path().join("blobs")).unwrap();
        let mut overlay = Overlay::default();
        let mut stage = Stage::default();
        let record = parked();
        save(
            &storage,
            &mut overlay,
            &mut stage,
            7,
            record.clone(),
            image(),
            true,
        )
        .unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &image_key(7)).unwrap(), None);
        let named = view.get(PARKED, &blob_key(7)).unwrap().unwrap();
        let held: BlobId = abi::decode(&named).unwrap();
        let line = namespace::line(PARKED, &frame_key(7));
        let on = namespace::on(&held, PARKED, &frame_key(7));
        assert_eq!(
            view.get(BLOBS, &line).unwrap(),
            Some(namespace::blob(&held))
        );
        assert_eq!(view.get(BLOBS, &on).unwrap(), Some(Vec::new()));
        assert!(view.get(BLOBS, &namespace::blob(&held)).unwrap().is_some());
        let layered = Layered {
            stage: &stage,
            blobs: &blobs,
        };
        assert_eq!(
            load(&view, &layered, 7).unwrap(),
            Some((record.clone(), image()))
        );
        save(
            &storage,
            &mut overlay,
            &mut stage,
            7,
            record.clone(),
            image(),
            true,
        )
        .unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(
            view.get(BLOBS, &line).unwrap(),
            Some(namespace::blob(&held))
        );
        assert!(view.get(BLOBS, &namespace::blob(&held)).unwrap().is_some());
        remove(&storage, &mut overlay, 7, &record.program).unwrap();
        let view = View::new(&storage, vec![&overlay]);
        assert_eq!(view.get(PARKED, &blob_key(7)).unwrap(), None);
        assert_eq!(view.get(BLOBS, &line).unwrap(), None);
        assert_eq!(view.get(BLOBS, &on).unwrap(), None);
        assert_eq!(view.get(BLOBS, &namespace::blob(&held)).unwrap(), None);
        assert_eq!(loaded(&view, 7), None);
    }
}
