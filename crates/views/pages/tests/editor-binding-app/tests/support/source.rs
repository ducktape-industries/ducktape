//! Synthetic mutable chunk producer for ingress interruption and snapshot tests.
use std::sync::Arc;
use ui_lang_wire as wire;
use wire::editor_document::{
    EditorDocumentRef, EditorTransfer, EditorTransferId, EditorTransferSender,
};

pub use pages_editor_binding_fixture::fixture_source::DocumentIdentity;

#[derive(Default)]
pub struct SourceStore {
    current: Option<Source>,
}
struct Source {
    identity: DocumentIdentity,
    reference: EditorDocumentRef,
    text: Arc<str>,
}

impl SourceStore {
    /// The caller supplies a connection-fenced page identity and advances reset
    /// only for an accepted source installation, never for a save echo.
    pub fn show(
        &mut self,
        identity: DocumentIdentity,
        text: &str,
        cursor: wire::EditorCursor,
    ) -> Result<Vec<u8>, &'static str> {
        let previous = self
            .current
            .as_ref()
            .filter(|source| source.identity == identity);
        let mut reference = EditorDocumentRef {
            document: identity.document.clone(),
            reset: identity.reset,
            revision: previous.map_or(0, |source| source.reference.revision),
            text_revision: previous.map_or(0, |source| source.reference.text_revision),
            byte_len: u32::try_from(text.len()).map_err(|_| "Document is too large")?,
            cursor,
        };
        reference
            .validate_text(text)
            .map_err(|_| "Document cannot be opened")?;
        if let Some(previous) = previous {
            let changed = previous.text.as_ref() != text;
            if changed || previous.reference.cursor != cursor {
                reference.revision = reference
                    .revision
                    .checked_add(1)
                    .ok_or("Document revision exhausted")?;
            }
            if changed {
                reference.text_revision = reference
                    .text_revision
                    .checked_add(1)
                    .ok_or("Document revision exhausted")?;
            }
        }
        let retained = previous
            .filter(|source| source.text.as_ref() == text)
            .map(|source| source.text.clone());
        self.current = Some(Source {
            identity: identity.clone(),
            reference,
            text: retained.unwrap_or_else(|| Arc::from(text)),
        });
        Ok(wire::encode(&identity))
    }

    pub fn transfer(
        &self,
        payload: &[u8],
        instance: u64,
        serial: u64,
    ) -> Result<Transfer, &'static str> {
        if payload.len() > 2048 {
            return Err("Invalid document source");
        }
        let identity: DocumentIdentity =
            wire::decode(payload).map_err(|_| "Invalid document source")?;
        let source = self
            .current
            .as_ref()
            .filter(|source| source.identity == identity)
            .ok_or("Document source changed")?;
        let id = EditorTransferId {
            instance,
            document: identity.document.clone(),
            reset: identity.reset,
            serial,
            attempt: 0,
        };
        let sender = EditorTransferSender::new(id, source.reference.clone())
            .map_err(|_| "Document cannot be opened")?;
        Ok(Transfer {
            sender: Some(sender),
            reference: source.reference.clone(),
            text: source.text.clone(),
        })
    }

    /// Check the installed editor against the exact source bytes we served.
    pub fn matches(&self, source: &[u8], text: &str) -> bool {
        let Ok(identity) = wire::decode::<DocumentIdentity>(source) else {
            return false;
        };
        self.current
            .as_ref()
            .is_some_and(|current| current.identity == identity && current.text.as_ref() == text)
    }

    pub fn clear(&mut self) {
        self.current = None;
    }
}

/// One existing wire chunk per redraw; no unbounded response vector or copied
/// document per chunk. A replaced or changed source explicitly aborts the stream.
pub struct Transfer {
    sender: Option<EditorTransferSender>,
    reference: EditorDocumentRef,
    text: Arc<str>,
}
impl Transfer {
    pub fn next(&mut self, sources: &SourceStore) -> Result<Option<EditorTransfer>, &'static str> {
        let Some(sender) = self.sender.as_mut() else {
            return Ok(None);
        };
        let current = sources.current.as_ref().map(|source| &source.reference);
        let Some(current) = current.filter(|current| **current == self.reference) else {
            let id = sender.id().clone();
            self.sender = None;
            return Ok(Some(EditorTransfer::Abort { id }));
        };
        sender
            .next_frame(current, &self.text)
            .map_err(|_| "Document transfer failed")
    }
}
