use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_BOARDS: usize = 64;
pub const MAX_SHAPES: usize = 128;
pub const MAX_TEXT: usize = 2048;
pub const MAX_COORD: i32 = 1_000_000;
pub const MAX_SIZE: i32 = 4000;
pub const MAX_BOARD_BYTES: usize = 768 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Note,
    Rectangle,
    Text,
    Arrow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    pub kind: Kind,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub text: String,
    pub color: u8,
    /// Arrow endpoints name shapes, so connections follow moved cards.
    pub from: Option<String>,
    pub to: Option<String>,
}
impl Default for Shape {
    fn default() -> Self {
        Self {
            kind: Kind::Note,
            x: 0,
            y: 0,
            width: 200,
            height: 140,
            text: String::new(),
            color: 0,
            from: None,
            to: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub revision: u64,
    pub shape: Shape,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Board {
    pub title: String,
    pub owner: String,
    pub revision: u64,
    pub shapes: BTreeMap<String, Record>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Create { id: String, title: String },
    Edit { board: String, change: Change },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Create { id: String, shape: Shape },
    Move { id: String, x: i32, y: i32 },
    Resize { id: String, width: i32, height: i32 },
    Text { id: String, text: String },
    Color { id: String, color: u8 },
    Delete { id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    List,
    Get { id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    List(BTreeMap<String, String>),
    Board(Option<Board>),
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
}
impl Board {
    pub fn new(title: String, owner: String) -> Result<Self, String> {
        let title_valid = !title.trim().is_empty() && title.len() <= 160;
        if !title_valid {
            return Err("Use a board name between 1 and 160 bytes.".into());
        }
        Ok(Self {
            title,
            owner,
            revision: 0,
            shapes: BTreeMap::new(),
        })
    }

    /// A pure reduction in consensus order. Field operations preserve unrelated
    /// concurrent edits; two writes to one field take the last ordered value.
    pub fn changed(&self, change: &Change) -> Result<Self, String> {
        match change {
            Change::Create { id, shape } => self.create(id, shape),
            Change::Move { id, x, y } => self.move_shape(id, *x, *y),
            Change::Resize { id, width, height } => self.resize(id, *width, *height),
            Change::Text { id, text } => self.text(id, text),
            Change::Color { id, color } => self.color(id, *color),
            Change::Delete { id } => self.delete(id),
        }
    }
    fn create(&self, id: &str, shape: &Shape) -> Result<Self, String> {
        if self.shapes.contains_key(id) {
            return Ok(self.clone());
        }
        if self.shapes.len() >= MAX_SHAPES {
            return Err(format!("A board supports up to {MAX_SHAPES} shapes."));
        }
        self.replace(id, Some(shape.clone()))
    }
    fn move_shape(&self, id: &str, x: i32, y: i32) -> Result<Self, String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(self.clone());
        };
        let mut shape = record.shape.clone();
        shape.x = x;
        shape.y = y;
        self.replace(id, Some(shape))
    }
    fn resize(&self, id: &str, width: i32, height: i32) -> Result<Self, String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(self.clone());
        };
        let mut shape = record.shape.clone();
        shape.width = width;
        shape.height = height;
        self.replace(id, Some(shape))
    }
    fn text(&self, id: &str, text: &str) -> Result<Self, String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(self.clone());
        };
        let mut shape = record.shape.clone();
        shape.text = text.to_owned();
        self.replace(id, Some(shape))
    }
    fn color(&self, id: &str, color: u8) -> Result<Self, String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(self.clone());
        };
        let mut shape = record.shape.clone();
        shape.color = color;
        self.replace(id, Some(shape))
    }
    fn delete(&self, id: &str) -> Result<Self, String> {
        if !self.shapes.contains_key(id) {
            return Ok(self.clone());
        }
        self.replace(id, None)
    }
    fn replace(&self, id: &str, shape: Option<Shape>) -> Result<Self, String> {
        if !valid_id(id) {
            return Err("Invalid shape id.".into());
        }
        if let Some(value) = &shape {
            self.validate_shape(id, value)?;
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("Board revision exhausted.")?;
        let mut next = self.clone();
        next.revision = revision;
        match shape {
            Some(shape) => {
                next.shapes
                    .insert(id.to_owned(), Record { revision, shape });
            }
            None => {
                next.shapes.remove(id);
                next.shapes.retain(|_, record| {
                    record.shape.from.as_deref() != Some(id)
                        && record.shape.to.as_deref() != Some(id)
                });
            }
        }
        let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_BOARD_BYTES {
            return Err("Board storage limit reached.".into());
        }
        Ok(next)
    }

    fn validate_shape(&self, id: &str, shape: &Shape) -> Result<(), String> {
        let geometry_valid = shape.x.abs_diff(0) <= MAX_COORD as u32
            && shape.y.abs_diff(0) <= MAX_COORD as u32
            && (40..=MAX_SIZE).contains(&shape.width)
            && (32..=MAX_SIZE).contains(&shape.height);
        let content_valid = shape.text.len() <= MAX_TEXT && shape.color < 5;
        if !geometry_valid || !content_valid {
            return Err("Shape exceeds the geometry or text limits.".into());
        }
        match shape.kind {
            Kind::Arrow => {
                let (Some(from), Some(to)) = (&shape.from, &shape.to) else {
                    return Err("Choose two shapes to connect.".into());
                };
                let endpoints_valid = from != to
                    && from != id
                    && to != id
                    && [from, to].iter().all(|key| {
                        self.shapes
                            .get(*key)
                            .is_some_and(|record| record.shape.kind != Kind::Arrow)
                    });
                if !endpoints_valid {
                    return Err("Connection endpoints must be existing cards.".into());
                }
            }
            Kind::Note | Kind::Rectangle | Kind::Text => {
                if shape.from.is_some() || shape.to.is_some() {
                    return Err("Only arrows have endpoints.".into());
                }
                let was_connected_card = self
                    .shapes
                    .get(id)
                    .is_some_and(|record| record.shape.kind == Kind::Arrow);
                if was_connected_card {
                    return Err("An arrow cannot become a card.".into());
                }
            }
        }
        let changing_to_arrow = shape.kind == Kind::Arrow
            && self
                .shapes
                .get(id)
                .is_some_and(|record| record.shape.kind != Kind::Arrow);
        if changing_to_arrow {
            return Err("A card cannot become an arrow.".into());
        }
        Ok(())
    }
}
