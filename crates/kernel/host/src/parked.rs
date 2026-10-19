//! The kernel's record of parked frames, in `$parked`: what each frame runs as and for whom, who
//! waits on it, the pages of its memory image, and the counters that number frames and tickets.
use abi::role::awaits::{FrameId, Ticket};
use abi::{BlobId, Cause, ItemRef, Origin, Principal, ProgramId, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use runtime::{Image, Page};
use state::{Overlay, Reads, View};

use crate::namespace::PARKED;
use crate::{Result, corrupt};

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

fn page_key(id: FrameId, index: u32) -> Vec<u8> {
    [b"page/".as_slice(), &id.to_be_bytes(), &index.to_be_bytes()].concat()
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

pub fn load(view: &View<'_>, id: FrameId) -> Result<Option<(Parked, Image)>> {
    let Some(parked) = record(view, id)? else {
        return Ok(None);
    };
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
    id: FrameId,
    parked: Parked,
    image: Image,
) -> Result<()> {
    drop_image(storage, overlay, id)?;
    let shape = Shape {
        length: image.length,
        pages: image.pages.iter().map(|page| page.index).collect(),
    };
    for page in image.pages {
        overlay.set(PARKED, page_key(id, page.index), page.bytes);
    }
    overlay.set(PARKED, shape_key(id), abi::encode(&shape));
    overlay.set(PARKED, of_key(&parked.program, id), Vec::new());
    overlay.set(PARKED, frame_key(id), abi::encode(&parked));
    Ok(())
}

pub fn remove(storage: &Reads, overlay: &mut Overlay, id: FrameId, program: &str) -> Result<()> {
    drop_image(storage, overlay, id)?;
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

fn drop_image(storage: &Reads, overlay: &mut Overlay, id: FrameId) -> Result<()> {
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
