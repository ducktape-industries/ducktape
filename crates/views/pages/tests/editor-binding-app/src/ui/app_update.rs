impl PagesEditorFixture {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Load => self.on_load(),
            Message::DocumentArrived(item) => self.on_document_arrived(item),
            Message::Committed(next) => self.on_committed(next),
            Message::DocumentTransaction(transaction) => self.on_document_transaction(transaction),
            Message::DocumentUpdated(document) => self.on_document_updated(document),
        }
    }
    fn on_load(&mut self) -> Task<Message> {
        self.source = crate::fixture::large_source();
        Task::none()
    }
    fn on_document_arrived(&mut self, item: crate::fixture_source::DocumentItem) -> Task<Message> {
        if item.source != self.source.reference {
            return Task::none();
        }
        self.load_error = item.error;
        if !self.load_error.is_empty() {
            return Task::none();
        }
        self.formatting_notice = item.notice;
        let reset = self.document.reset_revision();
        self.document.replace(
            crate::fixture_source::document_editor(item.text, item.cursor),
            reset,
        );
        self.installed_source = item.source;
        self.menu = crate::editor_binding::initial_menu();
        Task::none()
    }
    fn on_committed(&mut self, next: crate::editor_binding::EditorUpdate) -> Task<Message> {
        self.formatting_notice = next.notice;
        self.history = next.history;
        self.menu = next.menu;
        Task::none()
    }
    fn on_document_transaction(
        &mut self,
        transaction: ducktape_view_guest::EditorTransaction<Message>,
    ) -> Task<Message> {
        transaction
            .apply(&mut self.document)
            .map_or_else(Task::none, Task::done)
    }
    fn on_document_updated(
        &mut self,
        document: ducktape_view_guest::EditorDocumentUpdate,
    ) -> Task<Message> {
        document.apply(&mut self.document);
        Task::none()
    }
}
