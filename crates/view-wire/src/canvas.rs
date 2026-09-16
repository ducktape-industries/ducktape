//! Declarative geometry. Commands contain copied values, never host callbacks.
use crate::Rgba;
use serde::{Deserialize, Serialize};
use std::cell::Cell;

pub const MAX_CANVAS_PARTS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CanvasCommand {
    Draw {
        shape: CanvasShape,
        fill: Option<Rgba>,
        even_odd: bool,
        stroke: Option<CanvasStroke>,
    },
    Push {
        translate: [f32; 2],
        rotate: f32,
        scale: [f32; 2],
        clip: Option<[f32; 4]>,
    },
    Pop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CanvasShape {
    Rectangle {
        position: [f32; 2],
        size: [f32; 2],
        radius: [f32; 4],
    },
    Circle {
        center: [f32; 2],
        radius: f32,
    },
    Line {
        from: [f32; 2],
        to: [f32; 2],
    },
    Path(#[serde(deserialize_with = "decode_parts")] Vec<CanvasSegment>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CanvasSegment {
    Rectangle {
        position: [f32; 2],
        size: [f32; 2],
        radius: [f32; 4],
    },
    Circle {
        center: [f32; 2],
        radius: f32,
    },
    Move([f32; 2]),
    Line([f32; 2]),
    Arc {
        center: [f32; 2],
        radius: f32,
        start: f32,
        end: f32,
    },
    ArcTo {
        a: [f32; 2],
        b: [f32; 2],
        radius: f32,
    },
    Ellipse {
        center: [f32; 2],
        radius: [f32; 2],
        rotation: f32,
        start: f32,
        end: f32,
    },
    Bezier {
        a: [f32; 2],
        b: [f32; 2],
        end: [f32; 2],
    },
    Quadratic {
        control: [f32; 2],
        end: [f32; 2],
    },
    Close,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanvasStroke {
    pub color: Rgba,
    pub width: f32,
    pub cap: CanvasLineCap,
    pub join: CanvasLineJoin,
    #[serde(deserialize_with = "decode_parts")]
    pub dash: Vec<f32>,
    pub dash_offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasLineCap {
    Butt,
    Square,
    Round,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasLineJoin {
    Miter,
    Round,
    Bevel,
}

thread_local! { static DECODED: Cell<usize> = const { Cell::new(0) }; }
pub(super) fn reset_decode_budget() {
    DECODED.set(0);
}

/// Share a frame-wide allocation budget across command, path and dash lists.
/// Do not reserve from a sender-supplied collection length.
pub(super) fn decode_parts<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    struct Parts<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Parts<T> {
        type Value = Vec<T>;
        fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("bounded canvas geometry")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut parts = Vec::new();
            while let Some(part) = sequence.next_element::<T>()? {
                let count = DECODED.get();
                if count >= MAX_CANVAS_PARTS {
                    return Err(serde::de::Error::custom("canvas geometry budget exceeded"));
                }
                DECODED.set(count + 1);
                parts.push(part);
            }
            Ok(parts)
        }
    }
    deserializer.deserialize_seq(Parts(std::marker::PhantomData))
}

pub(super) fn sanitize(commands: &mut Vec<CanvasCommand>, budget: &mut usize) {
    commands.truncate((*budget).min(MAX_CANVAS_PARTS));
    let mut scales = vec![1.0_f32];
    let mut skipped = 0usize;
    commands.retain_mut(|command| {
        if skipped > 0 {
            match command {
                CanvasCommand::Push { .. } => skipped += 1,
                CanvasCommand::Pop => skipped -= 1,
                _ => {}
            }
            return false;
        }
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        match command {
            CanvasCommand::Push {
                translate,
                rotate,
                scale,
                clip,
            } => {
                if scales.len() >= 32 {
                    skipped = 1;
                    return false;
                }
                point(translate);
                angle(rotate);
                let parent = *scales.last().unwrap();
                for value in scale.iter_mut() {
                    *value = finite(*value).clamp(0.0, (8192.0 / parent).min(8192.0));
                }
                scales.push(if clip.is_some() {
                    1.0
                } else {
                    parent * scale[0].max(scale[1])
                });
                if let Some([x, y, width, height]) = clip {
                    coordinate(x);
                    coordinate(y);
                    size(width);
                    size(height);
                }
            }
            CanvasCommand::Pop => {
                if scales.len() == 1 {
                    return false;
                }
                scales.pop();
            }
            CanvasCommand::Draw {
                shape,
                fill,
                stroke,
                ..
            } => {
                if let Some(color) = fill {
                    rgba(color);
                }
                if let Some(stroke) = stroke {
                    rgba(&mut stroke.color);
                    size(&mut stroke.width);
                    stroke.dash.truncate((*budget).min(256));
                    *budget -= stroke.dash.len();
                    for value in &mut stroke.dash {
                        *value = finite(*value).clamp(0.01, 8192.0);
                    }
                    if !stroke.dash.is_empty() {
                        stroke.dash_offset %= stroke.dash.len() as u32;
                    }
                }
                match shape {
                    CanvasShape::Rectangle {
                        position,
                        size: dimensions,
                        radius,
                    } => {
                        point(position);
                        dimensions.iter_mut().for_each(size);
                        radius.iter_mut().for_each(size);
                    }
                    CanvasShape::Circle { center, radius } => {
                        point(center);
                        size(radius);
                    }
                    CanvasShape::Line { from, to } => {
                        point(from);
                        point(to);
                    }
                    CanvasShape::Path(segments) => {
                        segments.truncate(*budget);
                        *budget -= segments.len();
                        for segment in segments {
                            match segment {
                                CanvasSegment::Rectangle {
                                    position,
                                    size: dimensions,
                                    radius,
                                } => {
                                    point(position);
                                    dimensions.iter_mut().for_each(size);
                                    radius.iter_mut().for_each(size);
                                }
                                CanvasSegment::Circle { center, radius } => {
                                    point(center);
                                    size(radius);
                                }
                                CanvasSegment::Move(p) | CanvasSegment::Line(p) => point(p),
                                CanvasSegment::Arc {
                                    center,
                                    radius,
                                    start,
                                    end,
                                } => {
                                    point(center);
                                    size(radius);
                                    angle(start);
                                    angle(end);
                                }
                                CanvasSegment::ArcTo { a, b, radius } => {
                                    point(a);
                                    point(b);
                                    size(radius);
                                }
                                CanvasSegment::Ellipse {
                                    center,
                                    radius,
                                    rotation,
                                    start,
                                    end,
                                } => {
                                    point(center);
                                    radius.iter_mut().for_each(size);
                                    angle(rotation);
                                    angle(start);
                                    angle(end);
                                }
                                CanvasSegment::Bezier { a, b, end } => {
                                    point(a);
                                    point(b);
                                    point(end);
                                }
                                CanvasSegment::Quadratic { control, end } => {
                                    point(control);
                                    point(end);
                                }
                                CanvasSegment::Close => {}
                            }
                        }
                    }
                }
            }
        }
        true
    });
}
fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}
fn coordinate(value: &mut f32) {
    *value = finite(*value).clamp(-8192.0, 8192.0);
}
fn size(value: &mut f32) {
    *value = finite(*value).clamp(0.0, 8192.0);
}
fn angle(value: &mut f32) {
    *value = finite(*value).clamp(-std::f32::consts::TAU * 16.0, std::f32::consts::TAU * 16.0);
}
fn point(value: &mut [f32; 2]) {
    value.iter_mut().for_each(coordinate);
}
fn rgba(value: &mut Rgba) {
    for channel in &mut value.0 {
        *channel = finite(*channel).clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Node, decode, encode};

    fn node(commands: Vec<CanvasCommand>) -> Node {
        Node::Canvas {
            key: "geometry".into(),
            width: None,
            height: None,
            commands,
        }
    }
    fn path(parts: usize) -> CanvasCommand {
        CanvasCommand::Draw {
            shape: CanvasShape::Path(vec![CanvasSegment::Close; parts]),
            fill: Some(Rgba([1.0; 4])),
            even_odd: false,
            stroke: None,
        }
    }

    #[test]
    fn combined_native_scale_keeps_sub_epsilon_products() {
        let combined = f32::EPSILON * f32::EPSILON;
        let mut commands = vec![
            CanvasCommand::Push {
                translate: [0.0; 2],
                rotate: 0.0,
                scale: [combined; 2],
                clip: None,
            },
            CanvasCommand::Pop,
        ];
        let original = commands.clone();
        let mut budget = MAX_CANVAS_PARTS;
        sanitize(&mut commands, &mut budget);
        assert_eq!(
            commands, original,
            "a combined native transform must not be enlarged"
        );
    }

    #[test]
    fn decoder_counts_nested_parts_and_resets_after_rejection() {
        let oversized = encode(&node(vec![path(MAX_CANVAS_PARTS)]));
        assert!(
            decode::<Node>(&oversized).is_err(),
            "the command and its segments share a budget"
        );
        let valid = node(vec![path(MAX_CANVAS_PARTS - 1)]);
        let bytes = encode(&valid);
        assert_eq!(decode::<Node>(&bytes).unwrap(), valid);
        assert_eq!(decode::<Node>(&bytes).unwrap(), valid);
    }

    #[test]
    fn decoder_shares_budget_between_nodes() {
        let bytes = encode(&vec![node(vec![path(2047)]), node(vec![path(2048)])]);
        assert!(decode::<Vec<Node>>(&bytes).is_err());
    }

    #[test]
    fn sanitizer_shares_parts_budget_and_bounds_numbers() {
        let mut budget = MAX_CANVAS_PARTS;
        let mut first = vec![path(MAX_CANVAS_PARTS - 2)];
        sanitize(&mut first, &mut budget);
        let mut second = vec![
            CanvasCommand::Draw {
                shape: CanvasShape::Circle {
                    center: [f32::NAN, f32::MAX],
                    radius: -1.0,
                },
                fill: Some(Rgba([f32::INFINITY, -1.0, 2.0, 1.0])),
                even_odd: false,
                stroke: None,
            },
            path(10),
        ];
        sanitize(&mut second, &mut budget);
        assert_eq!(budget, 0);
        assert_eq!(
            second,
            vec![CanvasCommand::Draw {
                shape: CanvasShape::Circle {
                    center: [0.0, 8192.0],
                    radius: 0.0
                },
                fill: Some(Rgba([0.0, 0.0, 1.0, 1.0])),
                even_odd: false,
                stroke: None,
            }]
        );
    }

    #[test]
    fn sanitizer_discards_overdeep_groups_and_unmatched_pops() {
        let push = CanvasCommand::Push {
            translate: [0.0; 2],
            rotate: 0.0,
            scale: [8192.0; 2],
            clip: None,
        };
        let mut commands = vec![CanvasCommand::Pop];
        commands.extend(vec![push; 40]);
        commands.push(path(1));
        commands.extend(vec![CanvasCommand::Pop; 40]);
        commands.push(path(1));
        sanitize(&mut commands, &mut MAX_CANVAS_PARTS.clone());
        let mut depth = 0;
        let mut scale = 1.0;
        for command in &commands {
            match command {
                CanvasCommand::Push { scale: next, .. } => {
                    depth += 1;
                    scale *= next[0];
                    assert!(depth < 32);
                    assert!(scale <= 8192.0);
                }
                CanvasCommand::Pop => {
                    assert!(depth > 0);
                    depth -= 1;
                }
                CanvasCommand::Draw { .. } => {
                    assert_eq!(depth, 0, "overdeep subtree must be omitted")
                }
            }
        }
        assert_eq!(depth, 0);
        assert!(matches!(commands.last(), Some(CanvasCommand::Draw { .. })));
        let mut again = commands.clone();
        sanitize(&mut again, &mut MAX_CANVAS_PARTS.clone());
        assert_eq!(commands, again);
    }
}
