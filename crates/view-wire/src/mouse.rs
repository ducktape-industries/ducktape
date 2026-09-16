//! Mouse data in logical coordinates relative to the guest surface.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScrollDelta {
    Lines { x: f32, y: f32 },
    Pixels { x: f32, y: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    CursorEntered,
    CursorLeft,
    CursorMoved { x: f32, y: f32 },
    ButtonPressed(Button),
    ButtonReleased(Button),
    WheelScrolled { delta: ScrollDelta },
}

impl Event {
    /// Discards invalid numeric data without moving valid positions into bounds:
    /// a drag may legitimately continue outside the guest surface.
    pub fn sanitize(self) -> Option<Self> {
        let valid = match self {
            Self::CursorMoved { x, y }
            | Self::WheelScrolled {
                delta: ScrollDelta::Lines { x, y } | ScrollDelta::Pixels { x, y },
            } => x.is_finite() && y.is_finite(),
            _ => true,
        };
        valid.then_some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_signed_positions_buttons_and_wheel_units() {
        for event in [
            Event::CursorEntered,
            Event::CursorLeft,
            Event::CursorMoved { x: -12.5, y: 42.25 },
            Event::ButtonPressed(Button::Other(u16::MAX)),
            Event::ButtonReleased(Button::Back),
            Event::WheelScrolled {
                delta: ScrollDelta::Lines { x: -1.5, y: 3.25 },
            },
            Event::WheelScrolled {
                delta: ScrollDelta::Pixels { x: 7.5, y: -24.25 },
            },
        ] {
            assert_eq!(event.sanitize(), Some(event));
            let encoded = crate::encode(&event);
            let decoded: Event = crate::decode(&encoded).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn rejects_nonfinite_positions_and_scroll_deltas() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for event in [
                Event::CursorMoved { x: value, y: 0.0 },
                Event::CursorMoved { x: 0.0, y: value },
                Event::WheelScrolled {
                    delta: ScrollDelta::Lines { x: value, y: 0.0 },
                },
                Event::WheelScrolled {
                    delta: ScrollDelta::Pixels { x: 0.0, y: value },
                },
            ] {
                assert_eq!(event.sanitize(), None);
            }
        }
    }
}

/// Declarative native cursor, independent of window or OS handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cursor {
    None,
    Hidden,
    Idle,
    ContextMenu,
    Help,
    Pointer,
    Progress,
    Wait,
    Cell,
    Crosshair,
    Text,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    ResizingHorizontally,
    ResizingVertically,
    ResizingDiagonallyUp,
    ResizingDiagonallyDown,
    ResizingColumn,
    ResizingRow,
    AllScroll,
    ZoomIn,
    ZoomOut,
}

