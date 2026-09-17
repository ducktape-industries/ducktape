impl PagesEditorFixture {
    fn view(&self) -> wire::Node {
        use ducktape_view_guest::{kit, slots};
        let (document, on_document) = self
            .document
            .document("app:document".into(), Message::DocumentUpdated);
        let presentation = crate::presentation::paint(
            self.document.state_view(),
            self.menu.clone(),
            self.paint_dark,
            self.commented.clone(),
        );
        presentation
            .validate(self.document.state_view().text)
            .expect("invalid editor presentation");
        let mut children = vec![wire::Node::Editor {
            key: "PagesEditorFixture/document".into(),
            placeholder: String::new(),
            document,
            on_document,
            editable: true,
            width: Some(640.),
            height: None,
            min_height: Some(240.),
            max_height: Some(240.),
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(
                    crate::editor_binding::keys(
                        self.history.clone(),
                        self.menu.clone(),
                        Vec::new(),
                        Vec::new(),
                    )
                    .register(Message::Committed, Message::DocumentTransaction),
                )),
                presentation: Some(Box::new(presentation)),
                ..Default::default()
            }),
        }];
        if self.document.text().len() < 64000 {
            children.push(kit::text("PagesEditorFixture/echo", self.document.text()));
        }
        children.extend([
            kit::text(
                "PagesEditorFixture/formatting-notice",
                &self.formatting_notice,
            ),
            kit::text("PagesEditorFixture/load-error", &self.load_error),
            kit::button(
                "PagesEditorFixture/load",
                "Load document",
                Some(slots::message(Message::Load)),
                wire::ButtonPreset::Primary,
            ),
        ]);
        kit::sized(
            kit::column("PagesEditorFixture/layout", children),
            Some(wire::Length::Fixed(640.)),
            None,
        )
    }
}
