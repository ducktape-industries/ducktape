use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BOARDS: usize = 64;
pub const MAX_SHAPES: usize = 256;
pub const MAX_TEXT: usize = 2048;
pub const MAX_COORD: i32 = 1_000_000;
pub const MAX_SIZE: i32 = 4000;
pub const MAX_BOARD_BYTES: usize = 768 * 1024;
/// A stroke's samples. A pen drawn at screen resolution is simplified to fit.
pub const MAX_POINTS: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Note,
    Rectangle,
    Ellipse,
    Diamond,
    Text,
    Arrow,
    Line,
    Draw,
}
impl Kind {
    /// The two families a board holds. A card is a box that carries text; a
    /// path is a stroke through points. Nothing ever changes family: the
    /// fields that make sense are disjoint, and so is every rule below.
    pub fn is_path(self) -> bool {
        matches!(self, Kind::Arrow | Kind::Line | Kind::Draw)
    }
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
    /// A path's samples, relative to `x`/`y` and spanning `width`/`height`,
    /// so a move carries the stroke and a resize scales it. Cards hold none.
    pub points: Vec<[i32; 2]>,
    /// An arrow endpoint may name a card instead of standing on its own
    /// point, so the connection follows the card when it moves.
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
            points: Vec::new(),
            from: None,
            to: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub created: u64,
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
    Batch { board: String, changes: Vec<Change> },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Create {
        id: String,
        shape: Shape,
    },
    Move {
        id: String,
        x: i32,
        y: i32,
    },
    Resize {
        id: String,
        width: i32,
        height: i32,
    },
    Text {
        id: String,
        text: String,
    },
    Color {
        id: String,
        color: u8,
    },
    /// Raise these shapes, in this order, above everything else on the board.
    /// Naming every shape therefore states the whole stack — which is how a
    /// re-stack is undone exactly, rather than approximately.
    Order {
        ids: Vec<String>,
    },
    Delete {
        id: String,
    },
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
        self.changed_many(std::slice::from_ref(change))
    }
    /// One user gesture is atomic, including multi-selection and its undo.
    pub fn changed_many(&self, changes: &[Change]) -> Result<Self, String> {
        let bounded = !changes.is_empty() && changes.len() <= MAX_SHAPES * 2;
        if !bounded {
            return Err("An edit must contain between 1 and 256 changes.".into());
        }
        let mut next = self.clone();
        for change in changes {
            next.apply(change)?;
        }
        let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_BOARD_BYTES {
            return Err("Board storage limit reached.".into());
        }
        Ok(next)
    }
    fn apply(&mut self, change: &Change) -> Result<(), String> {
        match change {
            Change::Create { id, shape } => self.create(id, shape),
            Change::Move { id, x, y } => self.move_shape(id, *x, *y),
            Change::Resize { id, width, height } => self.resize(id, *width, *height),
            Change::Text { id, text } => self.text(id, text),
            Change::Color { id, color } => self.color(id, *color),
            Change::Order { ids } => self.order(ids),
            Change::Delete { id } => self.delete(id),
        }
    }
    pub fn ordered(&self) -> Vec<(&String, &Record)> {
        let mut shapes: Vec<_> = self.shapes.iter().collect();
        shapes.sort_by_key(|(id, record)| (record.created, *id));
        shapes
    }
    /// Stacking IS the creation order, so a re-stack renumbers it: the named
    /// shapes go on top in the order given, everything else keeps its own
    /// order underneath. Renumbering densely rather than hunting for a free
    /// stamp at one end leaves no gaps and no end to run out of.
    fn order(&mut self, ids: &[String]) -> Result<(), String> {
        let named: BTreeSet<&String> = ids.iter().collect();
        let addressable = named.len() == ids.len()
            && ids.len() <= MAX_SHAPES
            && ids.iter().all(|id| self.shapes.contains_key(id));
        if !addressable {
            return Err("Stacking names each shape on the board at most once.".into());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("Board revision exhausted.")?;
        self.revision = revision;
        let mut stack: Vec<String> = self
            .ordered()
            .into_iter()
            .map(|(id, _)| id.clone())
            .filter(|id| !named.contains(id))
            .collect();
        stack.extend(ids.iter().cloned());
        for (stamp, id) in stack.into_iter().enumerate() {
            if let Some(record) = self.shapes.get_mut(&id) {
                record.created = stamp as u64;
            }
        }
        Ok(())
    }
    fn create(&mut self, id: &str, shape: &Shape) -> Result<(), String> {
        if self.shapes.contains_key(id) {
            return Ok(());
        }
        if self.shapes.len() >= MAX_SHAPES {
            return Err(format!("A board supports up to {MAX_SHAPES} shapes."));
        }
        self.replace(id, Some(shape.clone()))
    }
    fn move_shape(&mut self, id: &str, x: i32, y: i32) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.x = x;
        shape.y = y;
        self.replace(id, Some(shape))
    }
    fn resize(&mut self, id: &str, width: i32, height: i32) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.width = width;
        shape.height = height;
        self.replace(id, Some(shape))
    }
    fn text(&mut self, id: &str, text: &str) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.text = text.to_owned();
        self.replace(id, Some(shape))
    }
    fn color(&mut self, id: &str, color: u8) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.color = color;
        self.replace(id, Some(shape))
    }
    fn delete(&mut self, id: &str) -> Result<(), String> {
        if !self.shapes.contains_key(id) {
            return Ok(());
        }
        self.replace(id, None)
    }
    fn replace(&mut self, id: &str, shape: Option<Shape>) -> Result<(), String> {
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
        self.revision = revision;
        match shape {
            Some(shape) => {
                let created = self
                    .shapes
                    .get(id)
                    .map_or(revision, |record| record.created);
                self.shapes.insert(
                    id.to_owned(),
                    Record {
                        created,
                        revision,
                        shape,
                    },
                );
            }
            None => {
                self.shapes.remove(id);
                self.shapes.retain(|_, record| {
                    record.shape.from.as_deref() != Some(id)
                        && record.shape.to.as_deref() != Some(id)
                });
            }
        }
        Ok(())
    }

    fn validate_shape(&self, id: &str, shape: &Shape) -> Result<(), String> {
        // A path's box is the span of its samples, so a straight horizontal
        // line is legitimately zero high; a card keeps a minimum both ways.
        let minimum = if shape.kind.is_path() {
            [0, 0]
        } else {
            [40, 32]
        };
        let geometry_valid = shape.x.abs_diff(0) <= MAX_COORD as u32
            && shape.y.abs_diff(0) <= MAX_COORD as u32
            && (minimum[0]..=MAX_SIZE).contains(&shape.width)
            && (minimum[1]..=MAX_SIZE).contains(&shape.height);
        let content_valid = shape.text.len() <= MAX_TEXT && shape.color < 5;
        if !geometry_valid || !content_valid {
            return Err("Shape exceeds the geometry or text limits.".into());
        }
        let swapping_family = self
            .shapes
            .get(id)
            .is_some_and(|record| record.shape.kind.is_path() != shape.kind.is_path());
        if swapping_family {
            return Err("A card and a connector are different shapes.".into());
        }
        if shape.kind.is_path() {
            self.validate_path(id, shape)
        } else {
            self.validate_card(shape)
        }
    }
    fn validate_card(&self, shape: &Shape) -> Result<(), String> {
        let bare = shape.points.is_empty() && shape.from.is_none() && shape.to.is_none();
        if !bare {
            return Err("Only connectors carry points or endpoints.".into());
        }
        Ok(())
    }
    fn validate_path(&self, id: &str, shape: &Shape) -> Result<(), String> {
        if !(2..=MAX_POINTS).contains(&shape.points.len()) {
            return Err(format!(
                "A connector needs between 2 and {MAX_POINTS} points."
            ));
        }
        let inside = shape
            .points
            .iter()
            .flatten()
            .all(|value| value.abs_diff(0) <= MAX_SIZE as u32);
        if !inside {
            return Err("Connector points must stay inside the shape.".into());
        }
        let bindable = shape.kind == Kind::Arrow;
        let bound = [&shape.from, &shape.to];
        if !bindable && bound.iter().any(|end| end.is_some()) {
            return Err("Only arrows bind to cards.".into());
        }
        if shape.from.is_some() && shape.from == shape.to {
            return Err("An arrow connects two different cards.".into());
        }
        let endpoints_valid = bound.into_iter().flatten().all(|key| {
            key != id
                && self
                    .shapes
                    .get(key)
                    .is_some_and(|record| !record.shape.kind.is_path())
        });
        if !endpoints_valid {
            return Err("An arrow binds to an existing card.".into());
        }
        Ok(())
    }
}
