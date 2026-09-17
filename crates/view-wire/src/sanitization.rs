//! Fixed-size reports from actual frame sanitization. No user text is retained.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizeReport {
    pub display_text_truncated: bool,
}
impl SanitizeReport {
    pub fn merge(&mut self, other: Self) {
        self.display_text_truncated |= other.display_text_truncated;
    }
}
