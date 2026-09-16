use view_wire::{Frame, Length, Node, decode, encode, sanitize};

#[test]
fn pin_preserves_local_offsets_and_bounds_untrusted_coordinates() {
    let pin = |x, y| Node::Pin {
        key: "pin".into(),
        x,
        y,
        width: Some(Length::Fixed(f32::INFINITY)),
        height: Some(Length::Fixed(-1.0)),
        content: Box::new(Node::empty()),
    };
    for (x, y, expected) in [
        (-4.0, 6.0, (-4.0, 6.0)),
        (f32::NAN, f32::INFINITY, (0.0, 8192.0)),
        (f32::NEG_INFINITY, 9000.0, (-8192.0, 8192.0)),
    ] {
        let mut frame = Frame {
            root: Some(pin(x, y)),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let node: Node = decode(&encode(&frame.root.unwrap())).unwrap();
        assert_eq!(node.key(), Some("pin"));
        assert_eq!(node.children().len(), 1);
        let Node::Pin {
            x,
            y,
            width,
            height,
            ..
        } = node
        else {
            unreachable!()
        };
        assert_eq!((x, y), expected);
        assert_eq!(width, Some(Length::Fixed(8192.0)));
        assert_eq!(height, Some(Length::Fixed(0.0)));
    }
}
