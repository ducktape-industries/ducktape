//! Copied data for host surfaces, with the same limits in both directions.
use serde::{Deserialize, Serialize};
use std::cell::Cell;

pub const MAX_SURFACE_DEPTH: usize = 32;
pub const MAX_SURFACE_VALUES: usize = 4096;

/// An owned, tagged value. Records carry the Ice declaration name and named
/// fields; native resources and pointers are not wire values.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum SurfaceValue {
    Unit,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    List(Vec<SurfaceValue>),
    Option(Option<Box<SurfaceValue>>),
    Record {
        name: String,
        fields: Vec<(String, SurfaceValue)>,
    },
}

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    static VALUES: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn reset_decode_budget() {
    VALUES.set(0);
}

impl<'de> Deserialize<'de> for SurfaceValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if DEPTH.get() >= MAX_SURFACE_DEPTH || VALUES.get() >= MAX_SURFACE_VALUES {
            return Err(serde::de::Error::custom("surface value budget exceeded"));
        }
        DEPTH.set(DEPTH.get() + 1);
        VALUES.set(VALUES.get() + 1);
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                DEPTH.set(DEPTH.get() - 1);
            }
        }
        let _guard = Guard;
        // Keep the serialization order identical to SurfaceValue. Recursive
        // fields enter the guarded public decoder before allocating children.
        #[derive(Deserialize)]
        enum Value {
            Unit,
            Bool(bool),
            I64(i64),
            F64(f64),
            Str(String),
            List(#[serde(deserialize_with = "decode_values")] Vec<SurfaceValue>),
            Option(Option<Box<SurfaceValue>>),
            Record {
                name: String,
                #[serde(deserialize_with = "decode_values")]
                fields: Vec<(String, SurfaceValue)>,
            },
        }
        Ok(match Value::deserialize(deserializer)? {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool(v),
            Value::I64(v) => Self::I64(v),
            Value::F64(v) => Self::F64(v),
            Value::Str(v) => Self::Str(v),
            Value::List(v) => Self::List(v),
            Value::Option(v) => Self::Option(v),
            Value::Record { name, fields } => Self::Record { name, fields },
        })
    }
}

// Do not reserve from untrusted length hints before decoding guarded values.
fn decode_values<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Values<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Values<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded surface values")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            if seq
                .size_hint()
                .is_some_and(|len| len > MAX_SURFACE_VALUES.saturating_sub(VALUES.get()))
            {
                return Err(serde::de::Error::custom(
                    "surface value budget exceeded by collection length",
                ));
            }
            let mut values = Vec::new();
            while let Some(value) = seq.next_element()? {
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Values(std::marker::PhantomData))
}

fn spend_name(name: &str, text: &mut usize) -> bool {
    if name.len() > (*text).min(super::MAX_STRING_BYTES) {
        return false;
    }
    *text -= name.len();
    true
}

/// Bound a provider's event before routing it. Invalid numeric values or
/// structural limits reject the whole event; strings retain the scalar
/// contract's UTF-8 truncation and share one total text budget.
pub fn sanitize_surface_event(value: &mut SurfaceValue) -> bool {
    let mut values = MAX_SURFACE_VALUES;
    let mut text = super::MAX_TEXT_BYTES_PER_FRAME;
    value.bound(0, &mut values, &mut text, true)
}

impl SurfaceValue {
    /// `event` rejects nonfinite values; guest frame arguments sanitize them
    /// to zero like scalar arguments. A failed argument is discarded by the
    /// caller rather than delivered as a partially truncated record/list.
    pub(super) fn bound(
        &mut self,
        depth: usize,
        values: &mut usize,
        text: &mut usize,
        event: bool,
    ) -> bool {
        if depth >= MAX_SURFACE_DEPTH || *values == 0 {
            return false;
        }
        *values -= 1;
        match self {
            Self::F64(v) if !v.is_finite() => {
                if event {
                    return false;
                }
                *v = 0.0;
            }
            Self::Str(v) => super::spend_text(v, text),
            Self::List(items) => {
                for item in items {
                    if !item.bound(depth + 1, values, text, event) {
                        return false;
                    }
                }
            }
            Self::Option(Some(item)) => return item.bound(depth + 1, values, text, event),
            Self::Record { name, fields } => {
                if !spend_name(name, text) {
                    return false;
                }
                for (name, item) in fields {
                    if !spend_name(name, text) {
                        return false;
                    }
                    if !item.bound(depth + 1, values, text, event) {
                        return false;
                    }
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn records_lists_and_options_round_trip_without_losing_tags() {
        let value = SurfaceValue::Record {
            name: "Row".into(),
            fields: vec![(
                "notes".into(),
                SurfaceValue::List(vec![
                    SurfaceValue::Option(None),
                    SurfaceValue::Option(Some(Box::new(SurfaceValue::Str("hello".into())))),
                ]),
            )],
        };
        assert_eq!(decode::<SurfaceValue>(&encode(&value)).unwrap(), value);
    }

    #[test]
    fn decode_limits_are_shared_and_reset_after_rejection() {
        let mut value = SurfaceValue::Unit;
        for _ in 0..MAX_SURFACE_DEPTH {
            value = SurfaceValue::Option(Some(Box::new(value)));
        }
        assert!(
            decode::<SurfaceValue>(&encode(&value))
                .unwrap_err()
                .contains("surface value budget")
        );
        let values = vec![SurfaceValue::Unit; MAX_SURFACE_VALUES + 1];
        assert!(
            decode::<Vec<SurfaceValue>>(&encode(&values))
                .unwrap_err()
                .contains("surface value budget")
        );
        assert_eq!(
            decode::<SurfaceValue>(&encode(&SurfaceValue::Unit)).unwrap(),
            SurfaceValue::Unit
        );
    }

    #[test]
    fn hostile_collection_lengths_fail_before_reading_elements() {
        let mut bytes = encode(&SurfaceValue::List(vec![]));
        bytes[4..12].copy_from_slice(&(MAX_SURFACE_VALUES as u64).to_le_bytes());
        assert!(
            decode::<SurfaceValue>(&bytes)
                .unwrap_err()
                .contains("collection length")
        );
    }

    #[test]
    fn structural_names_are_never_truncated_into_valid_names() {
        let mut value = SurfaceValue::Record {
            name: "Row".into(),
            fields: vec![
                (
                    "label".into(),
                    SurfaceValue::Str("x".repeat(crate::MAX_STRING_BYTES - 12)),
                ),
                ("notex".into(), SurfaceValue::Option(None)),
            ],
        };
        assert!(!sanitize_surface_event(&mut value));
        let SurfaceValue::Record { fields, .. } = value else {
            unreachable!()
        };
        assert_eq!(fields[1].0, "notex");
    }

    #[test]
    fn nested_events_bound_text_and_reject_nonfinite_numbers() {
        let mut value = SurfaceValue::List(vec![SurfaceValue::F64(f64::NAN)]);
        assert!(!sanitize_surface_event(&mut value));
        let mut value =
            SurfaceValue::List(vec![SurfaceValue::Str("é".repeat(crate::MAX_STRING_BYTES))]);
        assert!(sanitize_surface_event(&mut value));
        let SurfaceValue::List(values) = value else {
            unreachable!()
        };
        let SurfaceValue::Str(text) = &values[0] else {
            unreachable!()
        };
        assert!(text.len() <= crate::MAX_STRING_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }
}
