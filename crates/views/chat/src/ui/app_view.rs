use super::*;
use ducktape_view_guest::slots;
impl super::ChatView {
    pub(crate) fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
        let node_scope = format!("{}/chat", "ChatView");
        // Every press on the screen reports where it landed before the
        // control under it answers, so a menu opens at the pointer.
        let screen = wire::Node::MouseArea {
            key: format!("{node_scope}/press-area"),
            on_press: None,
            on_release: None,
            on_double_click: None,
            on_right_press: None,
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: None,
            on_exit: None,
            on_move: None,
            on_press_at: Some(slots::handler::<(f32, f32), Message>(Box::new(|(x, y)| {
                Some(Message::PressedAt(f64::from(x), f64::from(y)))
            }))),
            on_scroll: None,
            content: Box::new(self.chat_screen(node_scope.clone())),
        };
        // A message menu stacks over the screen: a transparent backdrop
        // that closes it on any press, then the card floated at the press.
        // (Not an `Overlay`: its modal wrapper paints a surface at the
        // un-translated origin, which a floated card leaves behind.)
        let mut layers = vec![screen];
        if let Some(menu) = self.floating_menu(&node_scope) {
            let close = self.close_menu();
            layers.push(wire::Node::MouseArea {
                key: format!("{node_scope}/menu-backdrop"),
                on_press: Some(slots::message(close.clone())),
                on_release: None,
                on_double_click: None,
                on_right_press: Some(slots::message(close)),
                on_right_release: None,
                on_middle_press: None,
                on_middle_release: None,
                on_enter: None,
                on_exit: None,
                on_move: None,
                on_press_at: None,
                on_scroll: None,
                content: Box::new(native::space(
                    Some(wire::Length::Fill),
                    Some(wire::Length::Fill),
                )),
            });
            layers.push(menu);
        }
        // `under: 1`: the screen lies under the in-flow backdrop; the menu
        // floats over both.
        let stack = wire::Node::Stack {
            key: format!("{node_scope}/menu-stack"),
            width: Some(wire::Length::Fill),
            height: Some(wire::Length::Fill),
            padding: None,
            background: None,
            border: None,
            clip: false,
            under: 1,
            children: layers,
        };
        // An open attachment previews in a modal card over everything: the
        // screen dims, and a press outside the card closes it.
        let overlay = match self.attachment_preview(&node_scope) {
            None => stack,
            Some(card) => wire::Node::Overlay {
                key: format!("{node_scope}/preview-overlay"),
                padding: 30.,
                backdrop: wire::Rgba([0., 0., 0., 0.55]),
                align_x: wire::AlignX::Center,
                align_y: wire::AlignY::Center,
                on_dismiss: Some(slots::message(Message::ClosePreview)),
                children: vec![stack, card],
            },
        };
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
            child: Box::new(overlay),
        }
    }
}
