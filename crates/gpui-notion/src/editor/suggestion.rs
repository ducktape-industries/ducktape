//! Suggestion menus: the `:` emoji picker and the `@` mention list.
//!
//! They share the slash menu's plumbing — a trigger character typed at a word
//! boundary, a query that grows until it stops matching, and Enter to commit —
//! so only the items and what committing does differ.

use gpui_kit::SharedString;

use super::mark::MarkKind;

/// What opened a suggestion menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `/` — the block command menu.
    Slash,
    /// `:` — emoji by shortcode.
    Emoji,
    /// `@` — mentions.
    Mention,
}

impl Trigger {
    pub fn character(&self) -> char {
        match self {
            Self::Slash => '/',
            Self::Emoji => ':',
            Self::Mention => '@',
        }
    }

    pub fn from_character(character: char) -> Option<Self> {
        match character {
            '/' => Some(Self::Slash),
            ':' => Some(Self::Emoji),
            '@' => Some(Self::Mention),
            _ => None,
        }
    }

    /// Placeholder shown while the query is empty.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Slash => "Filter...",
            Self::Emoji => "Add a emoji reaction...",
            Self::Mention => "Mention a person",
        }
    }

    /// The emoji menu needs a shortcode before it can match anything.
    pub fn opens_empty(&self) -> bool {
        !matches!(self, Self::Emoji)
    }
}

/// One emoji, keyed by the shortcodes people type for it.
pub struct Emoji {
    pub character: &'static str,
    pub name: &'static str,
    pub keywords: &'static [&'static str],
}

/// A small, opinionated set — enough to type with, and the place to plug a
/// full table in.
pub const EMOJI: &[Emoji] = &[
    Emoji { character: "😀", name: "grinning", keywords: &["smile", "happy"] },
    Emoji { character: "😉", name: "wink", keywords: &["joke"] },
    Emoji { character: "😍", name: "heart eyes", keywords: &["love"] },
    Emoji { character: "🤔", name: "thinking", keywords: &["hmm"] },
    Emoji { character: "🙌", name: "raised hands", keywords: &["celebrate", "yay"] },
    Emoji { character: "👍", name: "thumbs up", keywords: &["+1", "ok", "yes"] },
    Emoji { character: "👎", name: "thumbs down", keywords: &["-1", "no"] },
    Emoji { character: "🔥", name: "fire", keywords: &["hot", "lit"] },
    Emoji { character: "🚀", name: "rocket", keywords: &["ship", "launch"] },
    Emoji { character: "✅", name: "check", keywords: &["done", "tick"] },
    Emoji { character: "❌", name: "cross", keywords: &["no", "fail"] },
    Emoji { character: "⚠️", name: "warning", keywords: &["caution"] },
    Emoji { character: "💡", name: "bulb", keywords: &["idea", "note"] },
    Emoji { character: "📝", name: "memo", keywords: &["note", "write"] },
    Emoji { character: "🎉", name: "party", keywords: &["tada", "celebrate"] },
    Emoji { character: "❤️", name: "heart", keywords: &["love"] },
    Emoji { character: "🐛", name: "bug", keywords: &["issue", "defect"] },
    Emoji { character: "☕", name: "coffee", keywords: &["break"] },
];

/// Someone who can be mentioned. Applications replace this list.
#[derive(Clone, Debug)]
pub struct Mention {
    pub id: SharedString,
    pub name: SharedString,
}

impl Mention {
    pub fn new(id: impl Into<SharedString>, name: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }

    /// The mark a committed mention leaves on its text.
    pub fn mark(&self) -> MarkKind {
        MarkKind::Mention(self.id.clone())
    }
}

/// The people offered by the `@` menu, until an application sets its own.
pub fn default_mentions() -> Vec<Mention> {
    vec![
        Mention::new("ada", "Ada Lovelace"),
        Mention::new("alan", "Alan Turing"),
        Mention::new("grace", "Grace Hopper"),
        Mention::new("linus", "Linus Torvalds"),
        Mention::new("barbara", "Barbara Liskov"),
    ]
}

/// Case-insensitive contains, used by every menu's filter.
pub fn matches(query: &str, title: &str, keywords: &[&str]) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.trim().to_lowercase();
    title.to_lowercase().contains(&query)
        || keywords
            .iter()
            .any(|keyword| keyword.to_lowercase().contains(&query))
}
