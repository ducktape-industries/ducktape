impl PagesView {
    pub(crate) fn view(&self) -> wire::Node {
        measured(
            "PagesView/viewport",
            self.pages(),
            Message::PagesViewportChanged,
        )
    }
}
