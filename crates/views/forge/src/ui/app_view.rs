use super::*;
impl ForgeView {
    pub fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
        let viewport = || {
            ducktape_view_guest::slots::handler::<(f32, f32), Message>(Box::new(
                |(width, height)| Some(Message::ViewportChanged(width.into(), height.into())),
            ))
        };
        let content = self.forge_screen();
        wire::Node::Sensor {
            key: "ForgeView/viewport".into(),
            reset: None,
            on_show: Some(viewport()),
            on_resize: Some(viewport()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(native::page("ForgeView/page", [content])),
        }
    }
}
