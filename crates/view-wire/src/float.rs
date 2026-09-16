//! Copied translation arithmetic, evaluated against host-owned layout geometry.
use serde::{Deserialize, Serialize};

pub const MAX_FLOAT_OPS: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FloatExpression {
    #[serde(deserialize_with = "decode_ops")]
    pub ops: Vec<FloatOp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum FloatOp {
    Number(f64),
    /// Original x/y/width/height, followed by viewport x/y/width/height.
    Geometry(u8),
    Negate,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl FloatExpression {
    /// Invalid programs and non-finite translations do not move the child.
    pub fn evaluate(&self, geometry: [f64; 8]) -> f32 {
        self.checked_value(geometry).unwrap_or(0.0)
    }

    fn checked_value(&self, geometry: [f64; 8]) -> Option<f32> {
        if self.ops.len() > MAX_FLOAT_OPS {
            return None;
        }
        let mut stack = [0.0; MAX_FLOAT_OPS];
        let mut depth: usize = 0;
        for op in &self.ops {
            let value = match *op {
                FloatOp::Number(value) => value,
                FloatOp::Geometry(index) => *geometry.get(usize::from(index))?,
                FloatOp::Negate => {
                    depth = depth.checked_sub(1)?;
                    -stack[depth]
                }
                op => {
                    depth = depth.checked_sub(2)?;
                    let (a, b) = (stack[depth], stack[depth + 1]);
                    match op {
                        FloatOp::Add => a + b,
                        FloatOp::Subtract => a - b,
                        FloatOp::Multiply => a * b,
                        FloatOp::Divide => a / b,
                        FloatOp::Remainder => a % b,
                        _ => return None,
                    }
                }
            };
            if !value.is_finite() {
                return None;
            }
            stack[depth] = value;
            depth += 1;
        }
        let result = stack[0] as f32;
        (depth == 1 && result.is_finite()).then_some(result)
    }
}

fn decode_ops<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<FloatOp>, D::Error> {
    struct Ops;
    impl<'de> serde::de::Visitor<'de> for Ops {
        type Value = Vec<FloatOp>;
        fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("bounded float translation arithmetic")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut ops = Vec::new();
            while let Some(op) = seq.next_element()? {
                if ops.len() == MAX_FLOAT_OPS {
                    return Err(serde::de::Error::custom(
                        "float translation budget exceeded",
                    ));
                }
                ops.push(op);
            }
            Ok(ops)
        }
    }
    deserializer.deserialize_seq(Ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_uses_current_host_geometry_and_copied_values() {
        use FloatOp::*;
        let expression = FloatExpression {
            ops: vec![Geometry(6), Geometry(2), Subtract, Number(2.0), Divide],
        };
        assert_eq!(
            expression.evaluate([0.0, 0.0, 40.0, 20.0, 0.0, 0.0, 200.0, 80.0]),
            80.0
        );
        assert_eq!(
            expression.evaluate([0.0, 0.0, 40.0, 20.0, 0.0, 0.0, 300.0, 80.0]),
            130.0
        );
    }

    #[test]
    fn invalid_or_oversized_arithmetic_cannot_move_the_child() {
        use FloatOp::*;
        for ops in [
            vec![],
            vec![Add],
            vec![Geometry(8)],
            vec![Number(f64::NAN)],
            vec![Number(1.0), Number(0.0), Divide],
            vec![Number(1.0); MAX_FLOAT_OPS + 1],
        ] {
            assert_eq!(FloatExpression { ops }.evaluate([0.0; 8]), 0.0);
        }
    }

    #[test]
    fn decoding_rejects_translation_programs_over_the_budget() {
        let expression = FloatExpression {
            ops: vec![FloatOp::Number(1.0); MAX_FLOAT_OPS + 1],
        };
        let bytes = bincode::serialize(&expression).unwrap();
        let error = bincode::deserialize::<FloatExpression>(&bytes).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("float translation budget exceeded")
        );
    }
}
