//! The browser's pure model: where the reader stands and has been, what the
//! listing on hand is, how its rows are sorted and filtered, which row is
//! chosen, and what a key press or a finished write means. Nothing here
//! touches the host; the handlers in `app_update.rs` decide with these and
//! then write state.

use serde::{Deserialize, Serialize};

use crate::host::FsEntry;

/// Where the reader stands, and the trail behind and ahead of her — the
/// Back / Forward stacks a Finder window keeps. Every move goes through
/// [`Navigation::go`], [`Navigation::back`] and [`Navigation::forward`], so
/// there is one place the stacks change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Navigation {
    pub path: String,
    pub back: Vec<String>,
    pub forward: Vec<String>,
}

/// How deep the back stack may grow; older entries fall off the far end.
const TRAIL_DEPTH: usize = 64;

impl Navigation {
    pub fn at(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// A fresh navigation: the directory left goes on the back stack and the
    /// forward stack clears, as a browser's does. Standing still is no move.
    pub fn go(&mut self, target: &str) -> bool {
        let same_place = target == self.path;
        if same_place {
            return false;
        }
        self.back
            .push(std::mem::replace(&mut self.path, target.to_owned()));
        if self.back.len() > TRAIL_DEPTH {
            self.back.remove(0);
        }
        self.forward.clear();
        true
    }

    pub fn back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        self.forward
            .push(std::mem::replace(&mut self.path, previous));
        true
    }

    pub fn forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        self.back.push(std::mem::replace(&mut self.path, next));
        true
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub fn at_root(&self) -> bool {
        self.path == "/"
    }
}

/// The listing on hand for the current path. A failed read is a STATE the
/// pane draws, with Refresh beside it, never a loading spinner that stays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Listing {
    /// Asked, not yet answered.
    Pending,
    /// The rows of the pages walked so far; `next` is the cursor past them,
    /// "" once the directory ended.
    Listed { entries: Vec<FsEntry>, next: String },
    /// The node refused, and this is what it said.
    Failed(String),
}

impl Listing {
    pub fn entries(&self) -> &[FsEntry] {
        match self {
            Listing::Listed { entries, .. } => entries,
            Listing::Pending | Listing::Failed(_) => &[],
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Listing::Pending)
    }

    pub fn has_more(&self) -> bool {
        matches!(self, Listing::Listed { next, .. } if !next.is_empty())
    }
}

/// The two ways the main pane lays a directory out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    List,
    Columns,
}

/// Which column the rows are ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Size,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Sort {
    pub const BY_NAME: Sort = Sort {
        key: SortKey::Name,
        ascending: true,
    };

    /// Clicking the column already sorted by flips its direction; another
    /// column sorts ascending, as Finder's headers do.
    pub fn toggled(self, key: SortKey) -> Sort {
        let same_column = self.key == key;
        Sort {
            key,
            ascending: !same_column || !self.ascending,
        }
    }
}

/// The word a row's kind cell reads, from duckfs's own kind and the name's
/// extension: the only two facts the listing wire carries about a file.
pub fn kind_label(entry: &FsEntry) -> String {
    if entry.is_dir() {
        return "Folder".into();
    }
    if entry.kind == "symlink" {
        return "Link".into();
    }
    match crate::host::extension_of(&entry.path).as_str() {
        "" => "File".into(),
        "md" | "markdown" => "Markdown".into(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "Picture".into(),
        "txt" | "log" | "csv" => "Text".into(),
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "c" | "h" | "cpp" | "java" | "rb"
        | "sh" | "toml" | "yaml" | "yml" | "json" | "html" | "css" | "sql" | "wit" => "Code".into(),
        "wasm" | "zip" | "tar" | "gz" | "bin" | "pdf" => "Binary".into(),
        extension => extension.to_ascii_uppercase(),
    }
}

/// The glyph beside a row's name: one per kind, so a folder, a document, a
/// picture and a binary read apart at a glance.
pub fn kind_glyph(entry: &FsEntry) -> &'static str {
    match kind_label(entry).as_str() {
        "Folder" => "▰",
        "Link" => "↗",
        "Markdown" => "¶",
        "Picture" => "▣",
        "Text" => "≡",
        "Code" => "‹›",
        "Binary" => "▪",
        _ => "▫",
    }
}

/// The rows the pane shows: the listing's entries with the filter applied
/// and the sort imposed, folders first as Finder keeps them.
pub fn visible_rows(entries: &[FsEntry], filter: &str, sort: Sort) -> Vec<FsEntry> {
    let needle = filter.trim().to_lowercase();
    let mut rows: Vec<FsEntry> = entries
        .iter()
        .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        let folders_first = b.is_dir().cmp(&a.is_dir());
        let within = match sort.key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Kind => kind_label(a).cmp(&kind_label(b)),
        };
        let within = match sort.ascending {
            true => within,
            false => within.reverse(),
        };
        // a tie falls back to the name, ascending whatever the direction
        folders_first
            .then(within)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    rows
}

/// The row after `selected` in the direction of `step` (+1 down, -1 up),
/// the first row when nothing is selected, or the same row at the end.
pub fn neighbour(rows: &[FsEntry], selected: &str, step: i64) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let Some(index) = rows.iter().position(|row| row.path == selected) else {
        return Some(rows[0].path.clone());
    };
    let last = rows.len() as i64 - 1;
    let target = (index as i64 + step).clamp(0, last) as usize;
    Some(rows[target].path.clone())
}

/// A session-stable identity per path for keyed rendering: the same path is
/// the same row across every re-read, so the virtual list keeps its place.
/// A hash, not a registry — nothing to grow.
pub fn row_key(path: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish() as i64
}

/// What the reader is being asked to name: nothing, a new folder or file
/// under the current directory, or the new name of an existing entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamePrompt {
    Closed,
    NewFolder,
    NewFile,
    Rename(String),
}

impl NamePrompt {
    pub fn is_open(&self) -> bool {
        !matches!(self, NamePrompt::Closed)
    }
}

/// What a key press over the browser means, decided from the wire's key
/// state alone. Anything a focused control consumed never reaches here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowseKey {
    Up,
    Down,
    Left,
    Right,
    Open,
    Parent,
    Back,
    Forward,
    Cancel,
    Ignored,
}

pub fn browse_key(state: &ducktape_view_guest::wire::keyboard::KeyState) -> BrowseKey {
    use ducktape_view_guest::wire::keyboard::{Key, Named};
    let command = ducktape_view_guest::keyboard::command(state.modifiers);
    match &state.key {
        Key::Named(Named::ArrowUp) if command => BrowseKey::Parent,
        Key::Named(Named::ArrowUp) => BrowseKey::Up,
        Key::Named(Named::ArrowDown) if command => BrowseKey::Open,
        Key::Named(Named::ArrowDown) => BrowseKey::Down,
        Key::Named(Named::Enter) => BrowseKey::Open,
        Key::Named(Named::Escape) => BrowseKey::Cancel,
        Key::Named(Named::Backspace) => BrowseKey::Parent,
        Key::Named(Named::ArrowLeft) if command => BrowseKey::Back,
        Key::Named(Named::ArrowRight) if command => BrowseKey::Forward,
        Key::Named(Named::ArrowLeft) => BrowseKey::Left,
        Key::Named(Named::ArrowRight) => BrowseKey::Right,
        Key::Named(Named::BrowserBack) => BrowseKey::Back,
        Key::Named(Named::BrowserForward) => BrowseKey::Forward,
        _ => BrowseKey::Ignored,
    }
}

/// `12 items, 3 folders` — the status bar's tally.
pub fn tally(entries: &[FsEntry]) -> String {
    let folders = entries.iter().filter(|entry| entry.is_dir()).count();
    let items = entries.len();
    format!(
        "{} {}, {} {}",
        items,
        plural(items, "item", "items"),
        folders,
        plural(folders, "folder", "folders")
    )
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    match count {
        1 => one,
        _ => many,
    }
}
