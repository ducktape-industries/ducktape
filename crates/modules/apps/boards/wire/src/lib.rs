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
/// The whole of a card, as a [`Bond`] measures across it: an anchor is that
/// many thousandths from the card's top-left corner, so half of it is the
/// middle. Thousandths and not a float because every other number a shape
/// carries is an integer, and a board that mixed the two would round
/// differently depending on which field a reader asked.
pub const ANCHOR_SPAN: i32 = 1000;

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

/// Where a shape's words sit across the room they are written in. A card is a
/// column of text and a column can be read from either edge or from the
/// middle; nothing else about the shape changes with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Start,
    #[default]
    Middle,
    End,
}
/// Whether a shape's body is painted at all. A card washed in its colour says
/// "this is a thing"; a card with nothing behind its outline says "these are
/// the things inside me" — which is the gesture a whiteboard is for, and the
/// one a board that always washes cannot draw. A run has no body and ignores
/// it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    #[default]
    Solid,
    None,
}
/// Whether a shape's outline is drawn unbroken. Dashed is how every diagram
/// says "proposed", "optional", "not yet" — about a box and about an arrow
/// alike, which is why it is one property of every shape rather than a
/// connector's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
}
/// How heavy a shape's line is. Width is how a drawing says what is structure
/// and what is annotation — the box around the diagram drawn fat, the three
/// boxes inside it drawn thin — and you read that grouping before you read a
/// word of it. Without it a board has only colour to say it with, and colour
/// is already saying which KIND of thing each shape is, so one channel carries
/// two meanings and neither arrives.
///
/// Four steps and not a number, for the same reason [`TextSize`] is: a free
/// number lets a board hold a hairline no zoom can find and a slab that eats
/// the shape it outlines, and it makes "the same weight as that one" a thing
/// you match by eye instead of by picking it up.
///
/// Called weight and not width because a shape already HAS a width — the
/// geometric one, in board units, that a resize changes. This is the pen, not
/// the box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    Thin,
    #[default]
    Medium,
    Thick,
    Heavy,
}
/// Which ends of an arrow carry a head. "A ↔ B" is what you draw for a
/// dependency that goes both ways, a two-way sync, a negotiation, a bus — and
/// until this existed the board could not draw it at all, only lay two arrows
/// on top of each other that then moved, bound and deleted separately.
///
/// Three states and not two toggles, because it is ONE decision about a line
/// rather than two about its ends, and because the fourth combination the
/// toggles would offer — neither end — is already a shape you pick from the
/// toolbar. Two ways to say headless would be one too many.
///
/// Read only for [`Kind::Arrow`]. A line has no heads and is not asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Heads {
    /// A head where the arrow finished, which is every arrow drawn before this
    /// existed and the one a new arrow gets.
    #[default]
    End,
    Start,
    Both,
}
/// How big a shape's words are. Four steps and not a number: the size decides
/// the box a text shape hugs and the column a card wraps in, so a free number
/// would let a board hold writing that no zoom level can read and no box can
/// be fitted to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSize {
    Small,
    #[default]
    Medium,
    Large,
    Huge,
}

/// Which card a connector's end holds, and where on it. Both halves are one
/// fact: an anchor without a card names nothing, and a card without an anchor
/// is an arrow that forgets where you put it every time the card moves.
///
/// The place is stored against the CARD's own box and not against the board —
/// thousandths of its width and height from its top-left corner — so it
/// survives the card being moved and resized, which is the whole reason an end
/// binds to a card rather than standing on a point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bond {
    pub card: String,
    /// `[500, 500]` is the middle of the card, which is what an arrow dropped
    /// anywhere near the middle means: "this card", not "this spot on it".
    pub at: [i32; 2],
}
/// Which card an end holds, if it holds one — the question nearly every reader
/// of a connector's ends is actually asking.
pub fn held(end: &Option<Bond>) -> Option<&str> {
    end.as_ref().map(|bond| bond.card.as_str())
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
    /// Whether the body behind the outline is painted. A run has no body, so
    /// it is stored for every shape and read for the ones that have one.
    #[serde(default)]
    pub fill: Fill,
    /// Whether the outline is drawn unbroken — a card's border and a run's
    /// stroke are the same line as far as this is concerned.
    #[serde(default)]
    pub dash: Dash,
    /// How heavy that line is. Stored for every shape and read by every one
    /// that draws a line at all, which is every kind but text.
    #[serde(default)]
    pub weight: Weight,
    /// Which ends of it carry a head. Stored for every shape and read only by
    /// an arrow, the same way fill is stored for a line that has no body.
    #[serde(default)]
    pub heads: Heads,
    pub align: Align,
    pub text_size: TextSize,
    /// A path's samples, relative to `x`/`y` and spanning `width`/`height`,
    /// so a move carries the stroke and a resize scales it. Cards hold none.
    pub points: Vec<[i32; 2]>,
    /// An arrow endpoint may hold a card instead of standing on its own
    /// point, so the connection follows the card when it moves.
    pub from: Option<Bond>,
    pub to: Option<Bond>,
    /// Which group this shape belongs to, if any. A group is a NAME the
    /// members share and nothing else: there is no record of a group apart
    /// from the shapes in it, so a group cannot outlive its last member or be
    /// left dangling by a delete. Picking one member picks all of them.
    #[serde(default)]
    pub group: Option<String>,
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
            fill: Fill::Solid,
            dash: Dash::Solid,
            weight: Weight::Medium,
            heads: Heads::End,
            align: Align::Middle,
            text_size: TextSize::Medium,
            points: Vec::new(),
            from: None,
            to: None,
            group: None,
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
    Create {
        id: String,
        title: String,
    },
    /// The board under another name.
    ///
    /// Open to everyone, the way every shape edit already is. `Board::owner` is
    /// written once at creation and read in exactly one place — the idempotence
    /// check in `create` — and no edit has ever asked who is making it. Letting
    /// the author alone rename would be half an ownership model with no other
    /// half, and the name is what the rest of the network finds a board by.
    Rename {
        board: String,
        title: String,
    },
    /// The board, gone: out of the catalogue and out of the store.
    ///
    /// Only a board with NOTHING on it. That is not a stand-in for an ownership
    /// rule, it is the reason none is needed — a board with no shapes on it is
    /// a board nobody has done any work on, so removing one cannot take work
    /// away from anyone, whoever asks. A board someone has drawn on is refused
    /// outright: who may throw away another person's work is a question this
    /// module has never had an answer to, and a delete button is the wrong
    /// place to invent one.
    Remove {
        board: String,
    },
    Edit {
        board: String,
        change: Change,
    },
    Batch {
        board: String,
        changes: Vec<Change>,
    },
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
    Fill {
        id: String,
        fill: Fill,
    },
    Dash {
        id: String,
        dash: Dash,
    },
    Weight {
        id: String,
        weight: Weight,
    },
    Heads {
        id: String,
        heads: Heads,
    },
    Align {
        id: String,
        align: Align,
    },
    TextSize {
        id: String,
        text_size: TextSize,
    },
    /// A connector's run, restated: the samples, the box they span, and which
    /// cards its ends hold. Dragging one end moves all of them together — a
    /// re-route split into a move, a resize and a re-bind would be three undo
    /// steps, and three chances for a reader to see the arrow half-moved.
    Route {
        id: String,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        points: Vec<[i32; 2]>,
        from: Option<Bond>,
        to: Option<Bond>,
    },
    /// Raise these shapes, in this order, above everything else on the board.
    /// Naming every shape therefore states the whole stack — which is how a
    /// re-stack is undone exactly, rather than approximately.
    Order {
        ids: Vec<String>,
    },
    /// Bind these shapes into one group, or free them when the name is absent.
    /// One change for both directions, because they are one question — which
    /// group do these shapes belong to — and a board that grouped through one
    /// verb and ungrouped through another could answer it twice.
    ///
    /// Naming every member states the whole group, the way `Order` states the
    /// whole stack, so a grouping is undone exactly rather than approximately.
    Group {
        ids: Vec<String>,
        group: Option<String>,
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
/// The rule a board's name has to meet, in one place: a rename must not be able
/// to leave a board in a state a create would have refused.
pub fn valid_title(title: &str) -> Result<(), String> {
    let named = !title.trim().is_empty() && title.len() <= 160;
    match named {
        true => Ok(()),
        false => Err("Use a board name between 1 and 160 bytes.".into()),
    }
}
impl Board {
    pub fn new(title: String, owner: String) -> Result<Self, String> {
        valid_title(&title)?;
        Ok(Self {
            title,
            owner,
            revision: 0,
            shapes: BTreeMap::new(),
        })
    }
    /// The same board under another name, one revision on.
    ///
    /// The bump is what makes the new name reach a peer: a reader keeps the
    /// board it has until a revision at least as high arrives, and a rename
    /// that left the number alone would be a title that only landed on the next
    /// shape someone drew.
    pub fn renamed(&self, title: String) -> Result<Self, String> {
        valid_title(&title)?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("Board revision exhausted.")?;
        Ok(Self {
            title,
            revision,
            ..self.clone()
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
            Change::Fill { id, fill } => self.fill(id, *fill),
            Change::Dash { id, dash } => self.dash(id, *dash),
            Change::Weight { id, weight } => self.weight(id, *weight),
            Change::Heads { id, heads } => self.heads(id, *heads),
            Change::Align { id, align } => self.align(id, *align),
            Change::TextSize { id, text_size } => self.text_size(id, *text_size),
            Change::Route {
                id,
                x,
                y,
                width,
                height,
                points,
                from,
                to,
            } => self.route(id, [*x, *y, *width, *height], points, from, to),
            Change::Order { ids } => self.order(ids),
            Change::Group { ids, group } => self.regroup(ids, group.as_deref()),
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
    /// Put the named shapes in a group, or take them out of whatever group
    /// they were in. A group is only the name its members share, so freeing
    /// them is forgetting the name and there is nothing else to clean up.
    ///
    /// The whole change is refused if any part of it is, rather than half of
    /// a selection being grouped: a group that some of the shapes you picked
    /// are not in is not the group you asked for.
    fn regroup(&mut self, ids: &[String], group: Option<&str>) -> Result<(), String> {
        let named: BTreeSet<&String> = ids.iter().collect();
        let addressable = !ids.is_empty()
            && named.len() == ids.len()
            && ids.len() <= MAX_SHAPES
            && ids.iter().all(|id| self.shapes.contains_key(id));
        if !addressable {
            return Err("Grouping names each shape on the board at most once.".into());
        }
        // A group of one is allowed, and it has to be: freeing one member of a
        // pair leaves the other still carrying the name, and undoing that has
        // to be able to put it back. It reads no differently from no group at
        // all — picking it picks itself — so it is inert rather than invalid.
        // Refusing to MAKE one is a question about a gesture, and the view
        // answers it where the gesture is.
        if let Some(name) = group
            && !valid_id(name)
        {
            return Err("Use a group name of up to 96 id characters.".into());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("Board revision exhausted.")?;
        self.revision = revision;
        for id in ids {
            if let Some(record) = self.shapes.get_mut(id) {
                record.shape.group = group.map(str::to_owned);
                record.revision = revision;
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
    fn fill(&mut self, id: &str, fill: Fill) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.fill = fill;
        self.replace(id, Some(shape))
    }
    fn dash(&mut self, id: &str, dash: Dash) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.dash = dash;
        self.replace(id, Some(shape))
    }
    fn weight(&mut self, id: &str, weight: Weight) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.weight = weight;
        self.replace(id, Some(shape))
    }
    fn heads(&mut self, id: &str, heads: Heads) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.heads = heads;
        self.replace(id, Some(shape))
    }
    fn align(&mut self, id: &str, align: Align) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.align = align;
        self.replace(id, Some(shape))
    }
    fn text_size(&mut self, id: &str, text_size: TextSize) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        let mut shape = record.shape.clone();
        shape.text_size = text_size;
        self.replace(id, Some(shape))
    }
    /// A card has no run to re-route, so naming one here is a mistake worth
    /// hearing about rather than a no-op that silently keeps the old arrow.
    fn route(
        &mut self,
        id: &str,
        box_: [i32; 4],
        points: &[[i32; 2]],
        from: &Option<Bond>,
        to: &Option<Bond>,
    ) -> Result<(), String> {
        let Some(record) = self.shapes.get(id) else {
            return Ok(());
        };
        if !record.shape.kind.is_path() {
            return Err("Only a connector carries a run.".into());
        }
        let mut shape = record.shape.clone();
        shape.x = box_[0];
        shape.y = box_[1];
        shape.width = box_[2];
        shape.height = box_[3];
        shape.points = points.to_vec();
        shape.from = from.clone();
        shape.to = to.clone();
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
                    held(&record.shape.from) != Some(id) && held(&record.shape.to) != Some(id)
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
        // A group is a name shared by its members, so it has to be a name the
        // board can address — a shape carrying anything else names a group
        // nothing can ever be put in or taken out of.
        let group_valid = shape.group.as_deref().is_none_or(valid_id);
        if !group_valid {
            return Err("Use a group name of up to 96 id characters.".into());
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
        let holds_one_card = held(&shape.from).is_some() && held(&shape.from) == held(&shape.to);
        if holds_one_card {
            return Err("An arrow connects two different cards.".into());
        }
        let endpoints_valid = bound.into_iter().flatten().all(|bond| {
            bond.card != id
                && self
                    .shapes
                    .get(&bond.card)
                    .is_some_and(|record| !record.shape.kind.is_path())
        });
        if !endpoints_valid {
            return Err("An arrow binds to an existing card.".into());
        }
        // An anchor is a place ON the card, so it is meaningless outside it —
        // and a board that accepted one would draw an arrow ending in empty
        // space that no card could ever move.
        let anchors_valid = bound
            .into_iter()
            .flatten()
            .flat_map(|bond| bond.at)
            .all(|share| (0..=ANCHOR_SPAN).contains(&share));
        if !anchors_valid {
            return Err(format!(
                "An arrow's anchor sits between 0 and {ANCHOR_SPAN} of its card."
            ));
        }
        Ok(())
    }
}
