//! Copied flex rules; measurement and layout belong to the host.
use crate::{Edges, Length};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
    WrapReverse,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlexContentAlignment {
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Stretch,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlexItemAlignment {
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    Stretch,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum FlexBasis {
    #[default]
    Auto,
    Content,
    Fixed(f32),
    Percent(f32),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum FlexMargin {
    #[default]
    Zero,
    Auto,
    Fixed(f32),
    Percent(f32),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlexMargins {
    pub top: FlexMargin,
    pub right: FlexMargin,
    pub bottom: FlexMargin,
    pub left: FlexMargin,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlexLayout {
    pub direction: FlexDirection,
    pub wrap: FlexWrap,
    pub justify: Option<FlexContentAlignment>,
    pub items: Option<FlexItemAlignment>,
    pub content: Option<FlexContentAlignment>,
    pub row_gap: Option<f32>,
    pub column_gap: Option<f32>,
    pub padding: Option<Edges>,
    pub width: Option<Length>,
    pub height: Option<Length>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
    pub clip: bool,
    /// Utility sizing is also applied to the native outer painted container.
    pub surface_width: Option<Length>,
    pub surface_height: Option<Length>,
    pub surface_max_width: Option<f32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlexItem {
    pub order: i64,
    pub grow: Option<f32>,
    pub shrink: f32,
    pub basis: FlexBasis,
    pub align: Option<FlexItemAlignment>,
    pub margins: FlexMargins,
}
impl Default for FlexItem {
    fn default() -> Self {
        Self {
            order: 0,
            grow: None,
            shrink: 1.0,
            basis: FlexBasis::Auto,
            align: None,
            margins: FlexMargins::default(),
        }
    }
}

impl FlexLayout {
    pub(super) fn sanitize(&mut self) {
        for value in [
            &mut self.row_gap,
            &mut self.column_gap,
            &mut self.max_width,
            &mut self.max_height,
            &mut self.surface_max_width,
        ] {
            crate::bound_optional(value);
        }
        crate::bound_edges(&mut self.padding);
        for value in [
            &mut self.width,
            &mut self.height,
            &mut self.surface_width,
            &mut self.surface_height,
        ] {
            if let Some(Length::Fixed(pixels)) = value {
                *pixels = crate::bounded(*pixels);
            }
        }
    }
}
impl FlexItem {
    pub(super) fn sanitize(&mut self) {
        self.order = self.order.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        crate::bound_optional(&mut self.grow);
        self.shrink = crate::bounded(self.shrink);
        if let FlexBasis::Fixed(value) | FlexBasis::Percent(value) = &mut self.basis {
            *value = crate::bounded(*value);
        }
        for margin in [
            &mut self.margins.top,
            &mut self.margins.right,
            &mut self.margins.bottom,
            &mut self.margins.left,
        ] {
            if let FlexMargin::Fixed(value) | FlexMargin::Percent(value) = margin {
                *value = crate::finite(*value).clamp(-crate::MAX_PIXELS, crate::MAX_PIXELS);
            }
        }
    }
}

pub(super) fn decode_items<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<FlexItem>, D::Error> {
    struct Items;
    impl<'de> serde::de::Visitor<'de> for Items {
        type Value = Vec<FlexItem>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded flex item rules")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            if seq
                .size_hint()
                .is_some_and(|count| count > crate::MAX_DECODED_NODES)
            {
                return Err(serde::de::Error::custom("too many flex item rules"));
            }
            let mut items = Vec::new();
            while let Some(item) = seq.next_element()? {
                if items.len() == crate::MAX_DECODED_NODES {
                    return Err(serde::de::Error::custom("too many flex item rules"));
                }
                items.push(item);
            }
            Ok(items)
        }
    }
    deserializer.deserialize_seq(Items)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flex_metadata_tracks_children_after_the_shared_node_budget() {
        for (rows, rules) in [(3, 1), (2, 5), (crate::MAX_NODES + 3, 1)] {
            let mut node = crate::Node::Flex {
                key: "flex".into(),
                layout: FlexLayout::default(),
                background: None,
                border: None,
                items: vec![FlexItem::default(); rules],
                children: vec![crate::Node::empty(); rows],
            };
            crate::sanitize_tree(&mut node).unwrap();
            let crate::Node::Flex {
                items, children, ..
            } = node
            else {
                panic!("flex survives");
            };
            let expected = rows.min(crate::MAX_NODES - 1);
            assert_eq!(children.len(), expected);
            assert_eq!(
                items.len(),
                expected,
                "metadata must follow surviving children"
            );
        }
    }

    #[test]
    fn flex_item_length_prefix_is_rejected_before_reading_payloads() {
        #[derive(Debug, Deserialize)]
        struct Items(#[serde(deserialize_with = "decode_items")] Vec<FlexItem>);
        let small: Items =
            bincode::deserialize(&bincode::serialize(&vec![FlexItem::default()]).unwrap()).unwrap();
        assert_eq!(small.0, vec![FlexItem::default()]);
        let bytes = (crate::MAX_DECODED_NODES as u64 + 1).to_le_bytes();
        let error = bincode::deserialize::<Items>(&bytes).unwrap_err();
        assert!(
            error.to_string().contains("too many flex item rules"),
            "{error}"
        );
    }

    #[test]
    fn hostile_flex_rules_are_finite_and_bounded_without_losing_negative_margins() {
        let mut layout = FlexLayout {
            row_gap: Some(f32::NAN),
            column_gap: Some(f32::MAX),
            max_width: Some(f32::INFINITY),
            max_height: Some(-1.0),
            width: Some(Length::Fixed(f32::MAX)),
            padding: Some(Edges {
                top: f32::NAN,
                right: -2.0,
                bottom: f32::MAX,
                left: 3.0,
            }),
            ..FlexLayout::default()
        };
        layout.sanitize();
        assert_eq!(layout.row_gap, Some(0.0));
        assert_eq!(layout.column_gap, Some(crate::MAX_PIXELS));
        assert_eq!(layout.width, Some(Length::Fixed(crate::MAX_PIXELS)));
        assert_eq!(layout.max_width, Some(crate::MAX_PIXELS));
        assert_eq!(layout.max_height, Some(0.0));
        assert_eq!(
            layout.padding.unwrap(),
            Edges {
                top: 0.0,
                right: 0.0,
                bottom: crate::MAX_PIXELS,
                left: 3.0
            }
        );
        let mut item = FlexItem {
            order: i64::MAX,
            grow: Some(f32::INFINITY),
            shrink: -1.0,
            basis: FlexBasis::Fixed(f32::MAX),
            margins: FlexMargins {
                top: FlexMargin::Fixed(-12.0),
                right: FlexMargin::Fixed(f32::MIN),
                bottom: FlexMargin::Percent(f32::NAN),
                left: FlexMargin::Auto,
            },
            ..FlexItem::default()
        };
        item.sanitize();
        assert_eq!(item.order, i64::from(i32::MAX));
        assert_eq!(item.grow, Some(crate::MAX_PIXELS));
        assert_eq!(item.shrink, 0.0);
        assert_eq!(item.basis, FlexBasis::Fixed(crate::MAX_PIXELS));
        assert_eq!(item.margins.top, FlexMargin::Fixed(-12.0));
        assert_eq!(item.margins.right, FlexMargin::Fixed(-crate::MAX_PIXELS));
        assert_eq!(item.margins.bottom, FlexMargin::Percent(0.0));
        assert_eq!(item.margins.left, FlexMargin::Auto);
    }
}
