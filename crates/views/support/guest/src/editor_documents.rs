//! Document delivery is routed through the generated mutable Editor binding.
use crate::{Editor, slots, wire};
use wire::editor_document::{EditorDocumentMessage, EditorDocumentRef, EditorTransferError};

#[derive(Clone, Debug)]
pub struct EditorDocumentUpdate {
    document: String,
    message: EditorDocumentMessage,
}

impl EditorDocumentUpdate {
    pub fn apply(self, editor: &mut Editor) {
        let id = self.message.id().clone();
        if id.document != self.document {
            return;
        }
        let result = match self.message {
            EditorDocumentMessage::Request { id, target } => {
                let current = editor.document_reference(self.document);
                if target != current {
                    Err(EditorTransferError::Identity)
                } else {
                    slots::start_editor_transfer(id, target)
                }
            }
            EditorDocumentMessage::Acknowledged { .. } | EditorDocumentMessage::Failed { .. } => {
                slots::finish_editor_transfer(&id);
                Ok(())
            }
            EditorDocumentMessage::Transfer(transfer) => {
                match slots::receive_editor_mirror(&transfer) {
                    Ok(Some((text, target))) => {
                        if editor.install_mirror(text, &target) {
                            slots::acknowledge_editor_mirror(id.clone());
                            Ok(())
                        } else {
                            Err(EditorTransferError::Identity)
                        }
                    }
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                }
            }
        };
        if let Err(reason) = result {
            slots::editor_document_failure(id, reason);
        }
    }
}

impl Editor {
    /// Generated code calls this for every projection. The mirror stays owned
    /// by application state; routes and transfer progress retain only identity.
    pub fn document<M: 'static>(
        &self,
        document: String,
        wrap: impl Fn(EditorDocumentUpdate) -> M + 'static,
    ) -> (EditorDocumentRef, u32) {
        let reference = self.document_reference(document.clone());
        slots::editor_document_frame(&reference, self.text_ref());
        let handler = slots::handler::<EditorDocumentMessage, M>(Box::new(move |message| {
            Some(wrap(EditorDocumentUpdate {
                document: document.clone(),
                message,
            }))
        }));
        (reference, handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Driver, SnapshotApp};
    use std::cell::Cell;
    use wire::editor_document::{EditorTransfer, EditorTransferId};

    thread_local! { static ROUTE: Cell<u32> = const { Cell::new(0) }; }
    struct DocumentApp(Editor);
    impl App for DocumentApp {
        type Message = EditorDocumentUpdate;
        fn boot() -> (Self, crate::Task<Self::Message>) {
            (
                Self(Editor::new("x".repeat(wire::MAX_STRING_BYTES))),
                crate::Task::none(),
            )
        }
        fn view(&self) -> wire::Node {
            crate::memo_lazy(
                0,
                |_| {
                    let (_, route) = self.0.document("app:draft".into(), |update| update);
                    ROUTE.set(route);
                    wire::Node::empty()
                },
                1,
                "app",
                "document".into(),
            )
        }
        fn subscription(&self) -> crate::Subscription<Self::Message> {
            crate::Subscription::none()
        }
        fn update(&mut self, update: Self::Message) -> crate::Task<Self::Message> {
            update.apply(&mut self.0);
            crate::Task::none()
        }
    }
    impl SnapshotApp for DocumentApp {
        fn snapshot(&self) -> Result<Vec<u8>, String> {
            Ok(self.0.snapshot())
        }
        fn restore(bytes: &[u8]) -> Result<Self, String> {
            Editor::restore(bytes)
                .map(Self)
                .ok_or_else(|| "invalid document".into())
        }
    }

    #[test]
    fn cached_editor_view_progresses_without_messages_and_waits_for_exact_ack() {
        let mut driver = Driver::<DocumentApp>::new();
        driver.tick(vec![]);
        let target = driver.app.0.document_reference("app:draft".into());
        let id = EditorTransferId {
            instance: 9,
            document: target.document.clone(),
            reset: target.reset,
            serial: 4,
            attempt: 0,
        };
        let begin = driver.tick(vec![wire::Event::EditorDocument {
            handler: ROUTE.get(),
            message: EditorDocumentMessage::Request {
                id: id.clone(),
                target,
            },
        }]);
        assert!(matches!(
            &begin.editor_documents[..],
            [EditorDocumentMessage::Transfer(
                EditorTransfer::Begin { .. }
            )]
        ));
        assert!(driver.snapshot().is_err());
        let chunk = driver.tick(vec![]);
        assert!(
            matches!(&chunk.editor_documents[..], [EditorDocumentMessage::Transfer(EditorTransfer::Chunk { index: 0, bytes, .. })] if bytes.len() == wire::MAX_STRING_BYTES),
            "memoized view must produce the next bounded chunk"
        );
        let complete = driver.tick(vec![]);
        assert!(matches!(
            &complete.editor_documents[..],
            [EditorDocumentMessage::Transfer(
                EditorTransfer::Complete { .. }
            )]
        ));
        assert!(
            driver.snapshot().is_err(),
            "Complete is not a receiver acknowledgment"
        );
        let mut stale = id.clone();
        stale.serial -= 1;
        driver.tick(vec![wire::Event::EditorDocument {
            handler: u32::MAX,
            message: EditorDocumentMessage::Acknowledged { id: stale },
        }]);
        assert!(
            driver.snapshot().is_err(),
            "a stale acknowledgment cannot release current source progress"
        );
        driver.tick(vec![wire::Event::EditorDocument {
            handler: u32::MAX,
            message: EditorDocumentMessage::Acknowledged { id },
        }]);
        assert!(driver.snapshot().is_ok());
        assert!(driver.tick(vec![]).editor_documents.is_empty());
    }
}
