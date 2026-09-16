//! Bounded, copied container conditions. Evaluation never invokes guest code.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MAX_QUERY_OPS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerQuery {
    #[serde(deserialize_with = "decode_ops")]
    pub ops: Vec<QueryOp>,
}

/// Postfix operations keep decoding and evaluation iterative and bounded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum QueryOp {
    Width(String),
    Height(String),
    Number(f64),
    Bool(bool),
    Negate,
    Not,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
}

#[derive(Clone, Copy)]
enum Value {
    Number(f64),
    Bool(bool),
}

impl ContainerQuery {
    pub(super) fn sanitize(&mut self) {
        if self.ops.len() > MAX_QUERY_OPS
            || self.ops.iter().any(|op| match op {
                QueryOp::Number(n) => !n.is_finite(),
                QueryOp::Width(key) | QueryOp::Height(key) => key.len() > crate::MAX_STRING_BYTES,
                _ => false,
            })
        {
            self.ops.clear();
        }
    }

    /// A malformed condition never selects a branch.
    pub fn matches(&self, containers: &HashMap<String, [f64; 2]>) -> bool {
        self.evaluate(containers).unwrap_or(false)
    }

    fn evaluate(&self, containers: &HashMap<String, [f64; 2]>) -> Option<bool> {
        if self.ops.len() > MAX_QUERY_OPS {
            return None;
        }
        let mut stack = Vec::new();
        for op in &self.ops {
            let value = match op {
                QueryOp::Width(key) => Value::Number(containers.get(key)?[0]),
                QueryOp::Height(key) => Value::Number(containers.get(key)?[1]),
                QueryOp::Number(n) if n.is_finite() => Value::Number(*n),
                QueryOp::Number(_) => return None,
                QueryOp::Bool(b) => Value::Bool(*b),
                QueryOp::Negate => {
                    let Value::Number(n) = stack.pop()? else {
                        return None;
                    };
                    Value::Number(-n)
                }
                QueryOp::Not => {
                    let Value::Bool(b) = stack.pop()? else {
                        return None;
                    };
                    Value::Bool(!b)
                }
                op => {
                    let right = stack.pop()?;
                    let left = stack.pop()?;
                    match (left, right) {
                        (Value::Number(a), Value::Number(b)) => match op {
                            QueryOp::Add => Value::Number(a + b),
                            QueryOp::Subtract => Value::Number(a - b),
                            QueryOp::Multiply => Value::Number(a * b),
                            QueryOp::Divide => Value::Number(a / b),
                            QueryOp::Remainder => Value::Number(a % b),
                            QueryOp::Equal => Value::Bool(a == b),
                            QueryOp::NotEqual => Value::Bool(a != b),
                            QueryOp::Less => Value::Bool(a < b),
                            QueryOp::LessEqual => Value::Bool(a <= b),
                            QueryOp::Greater => Value::Bool(a > b),
                            QueryOp::GreaterEqual => Value::Bool(a >= b),
                            _ => return None,
                        },
                        (Value::Bool(a), Value::Bool(b)) => Value::Bool(match op {
                            QueryOp::And => a && b,
                            QueryOp::Or => a || b,
                            QueryOp::Equal => a == b,
                            QueryOp::NotEqual => a != b,
                            _ => return None,
                        }),
                        _ => return None,
                    }
                }
            };
            stack.push(value);
        }
        match stack.as_slice() {
            [Value::Bool(b)] => Some(*b),
            _ => None,
        }
    }
}

fn decode_ops<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<QueryOp>, D::Error> {
    struct Ops;
    impl<'de> serde::de::Visitor<'de> for Ops {
        type Value = Vec<QueryOp>;
        fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("a bounded container condition")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut ops = Vec::new();
            while let Some(op) = seq.next_element()? {
                if ops.len() == MAX_QUERY_OPS {
                    return Err(serde::de::Error::custom(
                        "container condition budget exceeded",
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
    fn sizes(width: f64, height: f64) -> HashMap<String, [f64; 2]> {
        HashMap::from([("panel".into(), [width, height])])
    }
    #[test]
    fn container_rules_use_both_dimensions_and_guest_thresholds() {
        use QueryOp::*;
        // width / 2 >= 300 && height < 500
        let query = ContainerQuery {
            ops: vec![
                Width("panel".into()),
                Number(2.0),
                Divide,
                Number(300.0),
                GreaterEqual,
                Height("panel".into()),
                Number(500.0),
                Less,
                And,
            ],
        };
        assert!(!query.matches(&sizes(599.0, 200.0)));
        assert!(query.matches(&sizes(600.0, 200.0)));
        assert!(!query.matches(&sizes(600.0, 500.0)));
    }
    #[test]
    fn malformed_or_oversized_rules_never_select() {
        use QueryOp::*;
        for ops in [
            vec![],
            vec![Bool(true), Bool(true)],
            vec![Width("panel".into()), And],
            vec![Number(f64::NAN), Number(0.0), NotEqual],
            vec![Width("panel".into()), Bool(true), Greater],
            vec![Bool(true); MAX_QUERY_OPS + 1],
        ] {
            assert!(!ContainerQuery { ops }.matches(&sizes(600.0, 200.0)));
        }
    }
    #[test]
    fn nested_rules_read_the_named_ancestor_and_refuse_unknown_containers() {
        use QueryOp::*;
        let query = ContainerQuery {
            ops: vec![Width("outer".into()), Width("inner".into()), Greater],
        };
        let containers = HashMap::from([
            ("outer".into(), [800.0, 600.0]),
            ("inner".into(), [300.0, 200.0]),
        ]);
        assert!(query.matches(&containers));
        assert!(!query.matches(&HashMap::from([("inner".into(), [300.0, 200.0])])));
    }
    #[test]
    fn frame_sanitization_discards_nonfinite_conditions() {
        let mut frame = crate::Frame {
            root: Some(crate::Node::When {
                key: "condition".into(),
                condition: ContainerQuery {
                    ops: vec![
                        QueryOp::Number(f64::INFINITY),
                        QueryOp::Number(0.0),
                        QueryOp::Greater,
                    ],
                },
                children: vec![crate::Node::empty()],
            }),
            ..crate::Frame::default()
        };
        crate::sanitize(&mut frame).unwrap();
        let Some(crate::Node::When {
            condition,
            children,
            ..
        }) = frame.root
        else {
            panic!("condition node")
        };
        assert!(condition.ops.is_empty());
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn decoder_refuses_oversized_rules_and_accepts_the_next_message() {
        let oversized = ContainerQuery {
            ops: vec![QueryOp::Bool(true); MAX_QUERY_OPS + 1],
        };
        assert!(crate::decode::<ContainerQuery>(&crate::encode(&oversized)).is_err());
        let valid = ContainerQuery {
            ops: vec![QueryOp::Bool(true)],
        };
        assert_eq!(
            crate::decode::<ContainerQuery>(&crate::encode(&valid)).unwrap(),
            valid
        );
    }
}
