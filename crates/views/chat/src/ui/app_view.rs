use super::*;
impl super::ChatView {
    pub(crate) fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
        wire::Node::Sensor {
            key: format!("{}/@sensor:906", "ChatView"),
            reset: None,
            on_show: Some(
                ::ducktape_view_guest::slots::handler::<(f32, f32), Message>(Box::new({
                    let route =
                        move |size: (f64, f64)| Message::ChatViewportChanged(size.0, size.1);
                    move |sent: (f32, f32)| Some(route((f64::from(sent.0), f64::from(sent.1))))
                })),
            ),
            on_resize: Some(
                ::ducktape_view_guest::slots::handler::<(f32, f32), Message>(Box::new({
                    let route =
                        move |size: (f64, f64)| Message::ChatViewportChanged(size.0, size.1);
                    move |sent: (f32, f32)| Some(route((f64::from(sent.0), f64::from(sent.1))))
                })),
            ),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new({
                let node_scope = format!("{}/chat", "ChatView");
                self.chat_screen(node_scope.clone())
            }),
        }
    }
}
