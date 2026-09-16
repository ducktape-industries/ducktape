//! Revisioned document transfer, independent of display text and native layout.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unicode_segmentation::GraphemeCursor;

use crate::EditorCursor;

pub const MAX_EDITOR_DOCUMENT_BYTES: usize = 1_048_576;
pub const MAX_EDITOR_CHUNK_BYTES: usize = 65_536;
pub const MAX_EDITOR_CHUNKS: usize = MAX_EDITOR_DOCUMENT_BYTES / MAX_EDITOR_CHUNK_BYTES;

// Aggregate caps are separate: shared logical text is charged once, while
// native widgets retain their own editable layout projections.
pub const MAX_EDITOR_DOCUMENTS: usize = 16;
pub const MAX_EDITOR_LIVE_BYTES: usize = 4 * MAX_EDITOR_DOCUMENT_BYTES;
pub const MAX_EDITOR_PROJECTION_BYTES: usize = 8 * MAX_EDITOR_DOCUMENT_BYTES;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorDocumentUsage {
    pub documents: usize,
    pub live_bytes: usize,
    pub projection_bytes: usize,
}

/// Validate all references before allocating projections or changing a live
/// document. Repeated bindings must describe exactly the same logical state.
pub fn validate_editor_document_refs<'a>(
    references: impl IntoIterator<Item = &'a EditorDocumentRef>,
) -> Result<EditorDocumentUsage, EditorTransferError> {
    let mut documents = HashMap::new();
    let mut usage = EditorDocumentUsage::default();
    for reference in references {
        reference.validate()?;
        let bytes = reference.byte_len as usize;
        usage.projection_bytes = usage
            .projection_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_EDITOR_PROJECTION_BYTES)
            .ok_or(EditorTransferError::Limit)?;
        match documents.get(reference.document.as_str()) {
            Some(previous) if *previous != reference => return Err(EditorTransferError::Identity),
            Some(_) => {}
            None => {
                if documents.len() == MAX_EDITOR_DOCUMENTS {
                    return Err(EditorTransferError::Limit);
                }
                usage.live_bytes = usage
                    .live_bytes
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_EDITOR_LIVE_BYTES)
                    .ok_or(EditorTransferError::Limit)?;
                documents.insert(reference.document.as_str(), reference);
            }
        }
    }
    usage.documents = documents.len();
    Ok(usage)
}

pub(crate) fn native_editor_boundary(text: &str, at: usize) -> bool {
    text.is_char_boundary(at)
        && !(at > 0
            && at < text.len()
            && matches!(&text.as_bytes()[at - 1..=at], b"\r\n" | b"\n\r"))
        && GraphemeCursor::new(at, text.len(), true)
            .is_boundary(text, 0)
            .unwrap_or(false)
}

/// The smallest changed span whose endpoints native Content can select.
/// Compare equal byte blocks first; query grapheme boundaries only at the edit,
/// instead of walking every grapheme in an unchanged one-MiB prefix or suffix.
pub fn editor_changed_span(
    before: &str,
    after: &str,
) -> Result<Vec<crate::EditorPatch>, crate::EditorPatchError> {
    if before.len() > MAX_EDITOR_DOCUMENT_BYTES || after.len() > MAX_EDITOR_DOCUMENT_BYTES {
        return Err(crate::EditorPatchError::Limit);
    }
    if before == after {
        return Ok(vec![]);
    }
    let limit = before.len().min(after.len());
    let mut start = 0;
    while start + 64 <= limit
        && before.as_bytes()[start..start + 64] == after.as_bytes()[start..start + 64]
    {
        start += 64;
    }
    while start < limit && before.as_bytes()[start] == after.as_bytes()[start] {
        start += 1;
    }
    while !native_editor_boundary(before, start) || !native_editor_boundary(after, start) {
        start -= 1;
    }
    let mut suffix = 0;
    let limit = limit - start;
    while suffix + 64 <= limit
        && before.as_bytes()[before.len() - suffix - 64..before.len() - suffix]
            == after.as_bytes()[after.len() - suffix - 64..after.len() - suffix]
    {
        suffix += 64;
    }
    while suffix < limit
        && before.as_bytes()[before.len() - suffix - 1]
            == after.as_bytes()[after.len() - suffix - 1]
    {
        suffix += 1;
    }
    while !native_editor_boundary(before, before.len() - suffix)
        || !native_editor_boundary(after, after.len() - suffix)
    {
        suffix -= 1;
    }
    Ok(vec![crate::EditorPatch {
        start_byte: start as u32,
        end_byte: (before.len() - suffix) as u32,
        replacement: after[start..after.len() - suffix].to_owned(),
    }])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorDocumentRef {
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub document: String,
    pub reset: u64,
    pub text_revision: u64,
    pub revision: u64,
    pub cursor: EditorCursor,
    pub byte_len: u32,
}

impl EditorDocumentRef {
    pub fn validate(&self) -> Result<(), EditorTransferError> {
        if self.document.is_empty()
            || self.document.len() > 1024
            || self.byte_len as usize > MAX_EDITOR_DOCUMENT_BYTES
        {
            return Err(EditorTransferError::Limit);
        }
        for position in std::iter::once(self.cursor.position).chain(self.cursor.selection) {
            if position.line > self.byte_len || position.column > self.byte_len {
                return Err(EditorTransferError::Cursor);
            }
        }
        Ok(())
    }

    pub fn validate_text(&self, text: &str) -> Result<(), EditorTransferError> {
        self.validate()?;
        if text.len() != self.byte_len as usize {
            return Err(EditorTransferError::Length);
        }
        let mut cursor = self.cursor;
        cursor.clamp(text);
        if cursor != self.cursor {
            return Err(EditorTransferError::Cursor);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorTransferId {
    pub instance: u64,
    #[serde(deserialize_with = "crate::editor_transaction::decode_document")]
    pub document: String,
    pub reset: u64,
    pub serial: u64,
    pub attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorTransfer {
    Begin {
        id: EditorTransferId,
        target: EditorDocumentRef,
    },
    Chunk {
        id: EditorTransferId,
        index: u8,
        #[serde(deserialize_with = "decode_chunk")]
        bytes: Vec<u8>,
    },
    Complete {
        id: EditorTransferId,
    },
    Abort {
        id: EditorTransferId,
    },
}

impl EditorTransfer {
    pub fn id(&self) -> &EditorTransferId {
        match self {
            Self::Begin { id, .. }
            | Self::Chunk { id, .. }
            | Self::Complete { id }
            | Self::Abort { id } => id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorTransferError {
    Limit,
    Identity,
    Order,
    Length,
    Utf8,
    Cursor,
    Aborted,
}

/// The same bounded exchange supplies an initial host projection and repairs a
/// guest mirror before a retained key is reconsidered. Routing is by exact id;
/// a reference alone does not authorize unsolicited bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorDocumentMessage {
    Request {
        id: EditorTransferId,
        target: EditorDocumentRef,
    },
    Transfer(EditorTransfer),
    Acknowledged {
        id: EditorTransferId,
    },
    Failed {
        id: EditorTransferId,
        reason: EditorTransferError,
    },
}

impl EditorDocumentMessage {
    pub fn id(&self) -> &EditorTransferId {
        match self {
            Self::Request { id, .. } | Self::Acknowledged { id } | Self::Failed { id, .. } => id,
            Self::Transfer(transfer) => transfer.id(),
        }
    }

    pub fn validate(&self) -> Result<(), EditorTransferError> {
        let id = self.id();
        if id.document.is_empty() || id.document.len() > 1024 {
            return Err(EditorTransferError::Identity);
        }
        let target = match self {
            Self::Request { target, .. } | Self::Transfer(EditorTransfer::Begin { target, .. }) => {
                Some(target)
            }
            Self::Transfer(EditorTransfer::Chunk { index, bytes, .. }) => {
                if usize::from(*index) >= MAX_EDITOR_CHUNKS
                    || bytes.is_empty()
                    || bytes.len() > MAX_EDITOR_CHUNK_BYTES
                {
                    return Err(EditorTransferError::Limit);
                }
                None
            }
            _ => None,
        };
        if let Some(target) = target {
            target.validate()?;
            if id.document != target.document || id.reset != target.reset {
                return Err(EditorTransferError::Identity);
            }
        }
        Ok(())
    }
}

pub(crate) fn decode_messages<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<EditorDocumentMessage>, D::Error> {
    let messages =
        crate::editor_transaction::decode_bounded::<D, EditorDocumentMessage, 1>(deserializer)?;
    for message in &messages {
        message
            .validate()
            .map_err(|_| serde::de::Error::custom("invalid editor document message"))?;
    }
    Ok(messages)
}

/// Transfer progress borrows the application's mirror only while producing one
/// frame. Queued senders retain metadata, never another full document copy.
#[derive(Debug)]
pub struct EditorTransferSender {
    id: EditorTransferId,
    target: EditorDocumentRef,
    stage: SendStage,
}

#[derive(Debug)]
enum SendStage {
    Begin,
    Chunk(usize),
    Ended,
}

impl EditorTransferSender {
    pub fn new(
        id: EditorTransferId,
        target: EditorDocumentRef,
    ) -> Result<Self, EditorTransferError> {
        target.validate()?;
        if id.document != target.document || id.reset != target.reset {
            return Err(EditorTransferError::Identity);
        }
        Ok(Self {
            id,
            target,
            stage: SendStage::Begin,
        })
    }

    pub fn id(&self) -> &EditorTransferId {
        &self.id
    }

    /// At most one chunk is allocated per call. A replaced source explicitly
    /// aborts the old transfer rather than sending bytes from two revisions.
    pub fn next_frame(
        &mut self,
        current: &EditorDocumentRef,
        text: &str,
    ) -> Result<Option<EditorTransfer>, EditorTransferError> {
        if matches!(self.stage, SendStage::Ended) {
            return Ok(None);
        }
        if current != &self.target {
            self.stage = SendStage::Ended;
            return Ok(Some(EditorTransfer::Abort {
                id: self.id.clone(),
            }));
        }
        if text.len() != self.target.byte_len as usize {
            self.stage = SendStage::Ended;
            return Err(EditorTransferError::Length);
        }
        let transfer = match self.stage {
            SendStage::Begin => {
                if let Err(error) = self.target.validate_text(text) {
                    self.stage = SendStage::Ended;
                    return Err(error);
                }
                self.stage = SendStage::Chunk(0);
                EditorTransfer::Begin {
                    id: self.id.clone(),
                    target: self.target.clone(),
                }
            }
            SendStage::Chunk(index) => {
                let start = index * MAX_EDITOR_CHUNK_BYTES;
                if start >= text.len() {
                    self.stage = SendStage::Ended;
                    EditorTransfer::Complete {
                        id: self.id.clone(),
                    }
                } else {
                    let end = (start + MAX_EDITOR_CHUNK_BYTES).min(text.len());
                    self.stage = SendStage::Chunk(index + 1);
                    EditorTransfer::Chunk {
                        id: self.id.clone(),
                        index: index as u8,
                        bytes: text.as_bytes()[start..end].to_vec(),
                    }
                }
            }
            SendStage::Ended => unreachable!("ended senders return before reading their source"),
        };
        Ok(Some(transfer))
    }
}

/// One bounded byte buffer, also usable by application-owned document loading.
/// No partial string can be observed. UTF-8 may cross any raw chunk boundary.
#[derive(Debug)]
pub struct EditorChunkAssembler {
    expected: usize,
    next: usize,
    bytes: Vec<u8>,
}

impl EditorChunkAssembler {
    pub fn new(byte_len: usize) -> Result<Self, EditorTransferError> {
        if byte_len > MAX_EDITOR_DOCUMENT_BYTES {
            return Err(EditorTransferError::Limit);
        }
        Ok(Self {
            expected: byte_len,
            next: 0,
            bytes: Vec::with_capacity(byte_len),
        })
    }

    pub fn push(&mut self, index: u8, bytes: &[u8]) -> Result<(), EditorTransferError> {
        if usize::from(index) != self.next || self.bytes.len() == self.expected {
            return Err(EditorTransferError::Order);
        }
        let expected = (self.expected - self.bytes.len()).min(MAX_EDITOR_CHUNK_BYTES);
        if bytes.len() != expected {
            return Err(EditorTransferError::Length);
        }
        self.bytes.extend_from_slice(bytes);
        self.next += 1;
        Ok(())
    }

    pub fn buffered_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn finish(self) -> Result<String, EditorTransferError> {
        if self.bytes.len() != self.expected
            || self.next != self.expected.div_ceil(MAX_EDITOR_CHUNK_BYTES)
        {
            return Err(EditorTransferError::Length);
        }
        String::from_utf8(self.bytes).map_err(|_| EditorTransferError::Utf8)
    }
}

/// A receiver is created only for an explicitly requested id and reference.
/// Wrong identities cannot discard its buffer. Malformed active transfers end
/// this receiver; an explicit retry must construct one with a new serial.
#[derive(Debug)]
pub struct EditorTransferReceiver {
    id: EditorTransferId,
    target: EditorDocumentRef,
    assembler: Option<EditorChunkAssembler>,
    ended: bool,
}

impl EditorTransferReceiver {
    pub fn new(
        id: EditorTransferId,
        target: EditorDocumentRef,
    ) -> Result<Self, EditorTransferError> {
        target.validate()?;
        if id.document != target.document || id.reset != target.reset {
            return Err(EditorTransferError::Identity);
        }
        Ok(Self {
            id,
            target,
            assembler: None,
            ended: false,
        })
    }

    pub fn buffered_bytes(&self) -> usize {
        self.assembler
            .as_ref()
            .map_or(0, EditorChunkAssembler::buffered_bytes)
    }

    pub fn receive(
        &mut self,
        transfer: &EditorTransfer,
    ) -> Result<Option<String>, EditorTransferError> {
        if transfer.id() != &self.id {
            return Err(EditorTransferError::Identity);
        }
        if self.ended {
            return Err(EditorTransferError::Order);
        }
        let result = self.receive_current(transfer);
        if result.is_err() {
            self.assembler = None;
            self.ended = true;
        }
        result
    }

    fn receive_current(
        &mut self,
        transfer: &EditorTransfer,
    ) -> Result<Option<String>, EditorTransferError> {
        match transfer {
            EditorTransfer::Begin { target, .. } => {
                if target != &self.target {
                    return Err(EditorTransferError::Identity);
                }
                if self.assembler.is_some() {
                    return Err(EditorTransferError::Order);
                }
                self.assembler = Some(EditorChunkAssembler::new(target.byte_len as usize)?);
                Ok(None)
            }
            EditorTransfer::Chunk { index, bytes, .. } => {
                self.assembler
                    .as_mut()
                    .ok_or(EditorTransferError::Order)?
                    .push(*index, bytes)?;
                Ok(None)
            }
            EditorTransfer::Complete { .. } => {
                self.ended = true;
                let text = self
                    .assembler
                    .take()
                    .ok_or(EditorTransferError::Order)?
                    .finish()?;
                self.target.validate_text(&text)?;
                Ok(Some(text))
            }
            EditorTransfer::Abort { .. } => Err(EditorTransferError::Aborted),
        }
    }
}

fn decode_chunk<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct Chunk;
    impl<'de> serde::de::Visitor<'de> for Chunk {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 64 KiB of raw editor bytes")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            if seq.size_hint().is_some_and(|n| n > MAX_EDITOR_CHUNK_BYTES) {
                return Err(serde::de::Error::custom("editor chunk byte limit"));
            }
            let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(byte) = seq.next_element()? {
                if bytes.len() == MAX_EDITOR_CHUNK_BYTES {
                    return Err(serde::de::Error::custom("editor chunk byte limit"));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
    d.deserialize_seq(Chunk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_allow_one_document_message_and_reject_a_second_before_delivery() {
        let (id, _) = metadata(MAX_EDITOR_DOCUMENT_BYTES);
        let message = EditorDocumentMessage::Transfer(EditorTransfer::Chunk {
            id,
            index: 0,
            bytes: vec![b'x'; MAX_EDITOR_CHUNK_BYTES],
        });
        let mut frame = crate::Frame {
            editor_documents: vec![message.clone()],
            ..Default::default()
        };
        let decoded: crate::Frame = crate::decode(&crate::encode(&frame)).unwrap();
        assert_eq!(decoded.editor_documents, frame.editor_documents);
        frame.editor_documents.push(message);
        assert!(
            crate::decode::<crate::Frame>(&crate::encode(&frame)).is_err(),
            "a frame cannot allocate a second document payload"
        );
    }

    #[test]
    fn document_messages_reject_cross_document_targets_and_unbounded_chunks() {
        let (id, target) = metadata(MAX_EDITOR_DOCUMENT_BYTES);
        let request = EditorDocumentMessage::Request {
            id: id.clone(),
            target: target.clone(),
        };
        assert_eq!(request.validate(), Ok(()));
        let mut wrong = target;
        wrong.reset += 1;
        assert_eq!(
            EditorDocumentMessage::Request {
                id: id.clone(),
                target: wrong
            }
            .validate(),
            Err(EditorTransferError::Identity)
        );
        for (index, length) in [(16, 1), (0, 0), (0, MAX_EDITOR_CHUNK_BYTES + 1)] {
            assert_eq!(
                EditorDocumentMessage::Transfer(EditorTransfer::Chunk {
                    id: id.clone(),
                    index,
                    bytes: vec![b'x'; length],
                })
                .validate(),
                Err(EditorTransferError::Limit)
            );
        }
        assert_eq!(
            EditorDocumentMessage::Acknowledged { id: id.clone() }.id(),
            &id
        );
    }

    #[test]
    fn repeated_bindings_charge_one_document_but_each_native_projection() {
        let (_, reference) = metadata(MAX_EDITOR_DOCUMENT_BYTES);
        let usage = validate_editor_document_refs(std::iter::repeat_n(&reference, 8)).unwrap();
        assert_eq!(
            usage,
            EditorDocumentUsage {
                documents: 1,
                live_bytes: MAX_EDITOR_DOCUMENT_BYTES,
                projection_bytes: MAX_EDITOR_PROJECTION_BYTES,
            }
        );
        assert_eq!(
            validate_editor_document_refs(std::iter::repeat_n(&reference, 9)),
            Err(EditorTransferError::Limit)
        );
        let mut conflicting = reference.clone();
        conflicting.revision += 1;
        assert_eq!(
            validate_editor_document_refs([&reference, &conflicting]),
            Err(EditorTransferError::Identity)
        );
    }

    #[test]
    fn independent_documents_have_separate_count_and_live_byte_limits() {
        let documents: Vec<_> = (0..=MAX_EDITOR_DOCUMENTS)
            .map(|index| {
                let (_, mut reference) = metadata(0);
                reference.document = format!("app:document-{index}");
                reference
            })
            .collect();
        assert_eq!(
            validate_editor_document_refs(&documents[..MAX_EDITOR_DOCUMENTS])
                .unwrap()
                .documents,
            MAX_EDITOR_DOCUMENTS
        );
        assert_eq!(
            validate_editor_document_refs(&documents),
            Err(EditorTransferError::Limit)
        );
        let mut full = documents[..5].to_vec();
        for reference in &mut full {
            reference.byte_len = MAX_EDITOR_DOCUMENT_BYTES as u32;
        }
        assert_eq!(
            validate_editor_document_refs(&full[..4])
                .unwrap()
                .live_bytes,
            MAX_EDITOR_LIVE_BYTES
        );
        assert_eq!(
            validate_editor_document_refs(&full),
            Err(EditorTransferError::Limit)
        );
    }

    #[test]
    fn sender_borrows_one_mib_and_delivers_one_bounded_chunk_per_frame() {
        let mut text = "x".repeat(MAX_EDITOR_DOCUMENT_BYTES - 3);
        text.insert(MAX_EDITOR_CHUNK_BYTES - 1, '한');
        let (id, target) = metadata(text.len());
        let mut sender = EditorTransferSender::new(id.clone(), target.clone()).unwrap();
        let mut receiver = EditorTransferReceiver::new(id, target.clone()).unwrap();
        let mut chunks = 0;
        let mut frames = 0;
        let mut result = None;
        while let Some(frame) = sender.next_frame(&target, &text).unwrap() {
            if let EditorTransfer::Chunk { index, bytes, .. } = &frame {
                assert_eq!(usize::from(*index), chunks, "each frame advances one chunk");
                assert!(bytes.len() <= MAX_EDITOR_CHUNK_BYTES);
                chunks += 1;
            }
            let delivered = receiver.receive(&frame).unwrap();
            if matches!(frame, EditorTransfer::Complete { .. }) {
                result = delivered;
            } else {
                assert!(delivered.is_none(), "no prefix is visible before Complete");
            }
            frames += 1;
            assert!(frames <= MAX_EDITOR_CHUNKS + 2);
        }
        assert_eq!(chunks, MAX_EDITOR_CHUNKS);
        assert_eq!(frames, MAX_EDITOR_CHUNKS + 2);
        assert_eq!(result.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn source_reset_aborts_an_incomplete_transfer_without_sending_new_bytes() {
        let text = "a".repeat(MAX_EDITOR_CHUNK_BYTES + 1);
        let (id, target) = metadata(text.len());
        let mut sender = EditorTransferSender::new(id.clone(), target.clone()).unwrap();
        assert!(matches!(
            sender.next_frame(&target, &text).unwrap(),
            Some(EditorTransfer::Begin { .. })
        ));
        assert!(matches!(
            sender.next_frame(&target, &text).unwrap(),
            Some(EditorTransfer::Chunk { index: 0, .. })
        ));
        let mut next = target;
        next.reset += 1;
        assert_eq!(
            sender.next_frame(&next, &text),
            Ok(Some(EditorTransfer::Abort { id }))
        );
        assert_eq!(sender.next_frame(&next, &text), Ok(None));
    }

    #[test]
    fn a_one_mib_document_sends_only_the_changed_byte_and_caret_sends_nothing() {
        let before = "a".repeat(MAX_EDITOR_DOCUMENT_BYTES - 1);
        let mut after = before.clone();
        let at = MAX_EDITOR_DOCUMENT_BYTES / 2;
        after.insert(at, 'X');
        let patches = editor_changed_span(&before, &after).unwrap();
        assert_eq!(
            patches,
            vec![crate::EditorPatch {
                start_byte: at as u32,
                end_byte: at as u32,
                replacement: "X".into(),
            }]
        );
        assert!(editor_changed_span(&after, &after).unwrap().is_empty());
        let mut observed = before;
        for patch in patches.iter().rev() {
            observed.replace_range(
                patch.start_byte as usize..patch.end_byte as usize,
                &patch.replacement,
            );
        }
        assert_eq!(observed, after);
    }

    #[test]
    fn minimal_spans_preserve_combining_emoji_and_paired_line_endings() {
        for (before, after, start, end, replacement) in [
            ("Ae\u{301}Z", "AeZ", 1, 4, "e"),
            ("A👍🏽Z", "A👍Z", 1, 9, "👍"),
            ("a\r\nb", "a\rX\nb", 1, 3, "\rX\n"),
            ("a\n\rb", "a\nX\rb", 1, 3, "\nX\r"),
            ("", "한", 0, 0, "한"),
            ("한", "", 0, 3, ""),
        ] {
            let patches = editor_changed_span(before, after).unwrap();
            assert_eq!(
                patches,
                vec![crate::EditorPatch {
                    start_byte: start,
                    end_byte: end,
                    replacement: replacement.into(),
                }],
                "{before:?} -> {after:?}"
            );
            assert_eq!(
                crate::patched_editor_text(before, &patches, EditorCursor::default()).unwrap(),
                after
            );
        }
    }

    fn metadata(len: usize) -> (EditorTransferId, EditorDocumentRef) {
        (
            EditorTransferId {
                instance: 3,
                document: "app:draft".into(),
                reset: 7,
                serial: 11,
                attempt: 0,
            },
            EditorDocumentRef {
                document: "app:draft".into(),
                reset: 7,
                text_revision: 2,
                revision: 4,
                cursor: EditorCursor::default(),
                byte_len: len as u32,
            },
        )
    }

    fn begun(len: usize) -> (EditorTransferId, EditorTransferReceiver) {
        let (id, target) = metadata(len);
        let mut receiver = EditorTransferReceiver::new(id.clone(), target.clone()).unwrap();
        assert_eq!(
            receiver.receive(&EditorTransfer::Begin {
                id: id.clone(),
                target
            }),
            Ok(None)
        );
        (id, receiver)
    }

    #[test]
    fn exact_one_mib_is_published_only_after_complete_even_when_utf8_crosses_a_chunk() {
        let mut text = "x".repeat(MAX_EDITOR_DOCUMENT_BYTES - 3);
        text.insert(MAX_EDITOR_CHUNK_BYTES - 1, '한');
        let (id, mut receiver) = begun(text.len());
        let chunks: Vec<_> = text.as_bytes().chunks(MAX_EDITOR_CHUNK_BYTES).collect();
        assert_eq!(chunks.len(), MAX_EDITOR_CHUNKS);
        assert!(std::str::from_utf8(chunks[0]).is_err());
        for (index, bytes) in chunks.iter().enumerate() {
            assert!(
                matches!(
                    receiver.receive(&EditorTransfer::Chunk {
                        id: id.clone(),
                        index: index as u8,
                        bytes: bytes.to_vec(),
                    }),
                    Ok(None)
                ),
                "a chunk must not publish a document prefix"
            );
            assert_eq!(
                receiver.buffered_bytes(),
                (index + 1) * MAX_EDITOR_CHUNK_BYTES
            );
        }
        assert_eq!(
            receiver.receive(&EditorTransfer::Complete { id }),
            Ok(Some(text))
        );
        assert_eq!(receiver.buffered_bytes(), 0);
    }

    #[test]
    fn every_interruption_boundary_discards_staging_without_publishing_a_prefix() {
        for boundary in 0..=MAX_EDITOR_CHUNKS {
            let (id, mut receiver) = begun(MAX_EDITOR_DOCUMENT_BYTES);
            for index in 0..boundary {
                assert_eq!(
                    receiver.receive(&EditorTransfer::Chunk {
                        id: id.clone(),
                        index: index as u8,
                        bytes: vec![b'x'; MAX_EDITOR_CHUNK_BYTES],
                    }),
                    Ok(None)
                );
            }
            assert_eq!(
                receiver.receive(&EditorTransfer::Abort { id: id.clone() }),
                Err(EditorTransferError::Aborted)
            );
            assert_eq!(receiver.buffered_bytes(), 0);
            assert_eq!(
                receiver.receive(&EditorTransfer::Complete { id }),
                Err(EditorTransferError::Order)
            );
        }
    }

    #[test]
    fn stale_identity_cannot_abort_or_append_to_the_requested_document() {
        let (id, mut receiver) = begun(MAX_EDITOR_CHUNK_BYTES + 1);
        assert_eq!(
            receiver.receive(&EditorTransfer::Chunk {
                id: id.clone(),
                index: 0,
                bytes: vec![b'a'; MAX_EDITOR_CHUNK_BYTES],
            }),
            Ok(None)
        );
        for change in 0..4 {
            let mut stale = id.clone();
            match change {
                0 => stale.instance += 1,
                1 => stale.serial += 1,
                2 => stale.reset += 1,
                _ => stale.document = "app:another".into(),
            }
            for event in [
                EditorTransfer::Abort { id: stale.clone() },
                EditorTransfer::Chunk {
                    id: stale.clone(),
                    index: 1,
                    bytes: vec![b'b'],
                },
                EditorTransfer::Complete { id: stale },
            ] {
                assert_eq!(receiver.receive(&event), Err(EditorTransferError::Identity));
                assert_eq!(receiver.buffered_bytes(), MAX_EDITOR_CHUNK_BYTES);
            }
        }
        receiver
            .receive(&EditorTransfer::Chunk {
                id: id.clone(),
                index: 1,
                bytes: vec![b'b'],
            })
            .unwrap();
        let text = receiver
            .receive(&EditorTransfer::Complete { id })
            .unwrap()
            .unwrap();
        assert_eq!(text, format!("{}b", "a".repeat(MAX_EDITOR_CHUNK_BYTES)));
    }

    #[test]
    fn malformed_active_transfer_fails_closed_instead_of_becoming_a_partial_document() {
        for bad in 0..4 {
            let (id, mut receiver) = begun(MAX_EDITOR_CHUNK_BYTES + 1);
            receiver
                .receive(&EditorTransfer::Chunk {
                    id: id.clone(),
                    index: 0,
                    bytes: vec![b'a'; MAX_EDITOR_CHUNK_BYTES],
                })
                .unwrap();
            let event = match bad {
                0 => EditorTransfer::Chunk {
                    id: id.clone(),
                    index: 0,
                    bytes: vec![b'a'; MAX_EDITOR_CHUNK_BYTES],
                },
                1 => EditorTransfer::Chunk {
                    id: id.clone(),
                    index: 2,
                    bytes: vec![b'b'],
                },
                2 => EditorTransfer::Chunk {
                    id: id.clone(),
                    index: 1,
                    bytes: vec![b'b'; 2],
                },
                _ => EditorTransfer::Complete { id: id.clone() },
            };
            assert!(receiver.receive(&event).is_err());
            assert_eq!(receiver.buffered_bytes(), 0);
            assert_eq!(
                receiver.receive(&EditorTransfer::Complete { id }),
                Err(EditorTransferError::Order)
            );
        }
    }

    #[test]
    fn complete_checks_utf8_and_native_cursor_and_empty_documents_need_no_chunk() {
        let (id, mut receiver) = begun(2);
        receiver
            .receive(&EditorTransfer::Chunk {
                id: id.clone(),
                index: 0,
                bytes: vec![0xff, 0xff],
            })
            .unwrap();
        assert_eq!(
            receiver.receive(&EditorTransfer::Complete { id }),
            Err(EditorTransferError::Utf8)
        );
        let (id, mut target) = metadata(3);
        target.cursor.position.column = 1;
        let mut receiver = EditorTransferReceiver::new(id.clone(), target.clone()).unwrap();
        receiver
            .receive(&EditorTransfer::Begin {
                id: id.clone(),
                target,
            })
            .unwrap();
        receiver
            .receive(&EditorTransfer::Chunk {
                id: id.clone(),
                index: 0,
                bytes: "e\u{301}".as_bytes().to_vec(),
            })
            .unwrap();
        assert_eq!(
            receiver.receive(&EditorTransfer::Complete { id }),
            Err(EditorTransferError::Cursor)
        );
        let (id, mut receiver) = begun(0);
        assert_eq!(
            receiver.receive(&EditorTransfer::Complete { id }),
            Ok(Some(String::new()))
        );
    }

    #[test]
    fn reference_limits_are_checked_before_allocating_a_document() {
        let (id, mut target) = metadata(MAX_EDITOR_DOCUMENT_BYTES + 1);
        assert_eq!(
            EditorTransferReceiver::new(id.clone(), target.clone()).unwrap_err(),
            EditorTransferError::Limit
        );
        target.byte_len = 0;
        target.document = "d".repeat(1025);
        assert_eq!(target.validate(), Err(EditorTransferError::Limit));
        let (_, mut target) = metadata(0);
        target.cursor.position.line = 1;
        assert_eq!(target.validate(), Err(EditorTransferError::Cursor));
        target.cursor = EditorCursor::default();
        target.reset += 1;
        assert_eq!(
            EditorTransferReceiver::new(id, target).unwrap_err(),
            EditorTransferError::Identity
        );
        assert!(EditorChunkAssembler::new(MAX_EDITOR_DOCUMENT_BYTES + 1).is_err());
    }

    #[test]
    fn decoder_rejects_advertised_oversized_chunks_before_reading_their_payload() {
        let (id, _) = metadata(0);
        let event = EditorTransfer::Chunk {
            id,
            index: 0,
            bytes: vec![],
        };
        let mut encoded = crate::encode(&event);
        let end = encoded.len();
        encoded[end - 8..].copy_from_slice(&((MAX_EDITOR_CHUNK_BYTES + 1) as u64).to_le_bytes());
        let error = crate::decode::<EditorTransfer>(&encoded)
            .unwrap_err()
            .to_string();
        assert!(error.contains("editor chunk byte limit"), "{error}");
    }
}

#[cfg(test)]
mod boundary_query_tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn direct_queries_match_native_boundaries_for_context_sensitive_unicode() {
        for text in [
            "",
            "a\r\nb\n\rc",
            "e\u{301}",
            "🇰🇷🇨🇦🇺🇸🇬",
            "👩🏽‍👩‍👧‍👦",
            "\u{600}a",
            "क्‍ष",
        ] {
            let expected: Vec<_> = text
                .grapheme_indices(true)
                .map(|(at, _)| at)
                .chain(std::iter::once(text.len()))
                .filter(|at| {
                    !(*at > 0
                        && *at < text.len()
                        && matches!(&text.as_bytes()[at - 1..=*at], b"\r\n" | b"\n\r"))
                })
                .collect();
            for at in 0..=text.len() + 1 {
                assert_eq!(
                    native_editor_boundary(text, at),
                    expected.contains(&at),
                    "{text:?} byte {at}"
                );
            }
        }
    }
}
