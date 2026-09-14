//! Why the launcher stopped short of what it was told: a stable snake_case
//! `reason` (the log field a dashboard greps and counts) and a human
//! `detail` for stderr. Never argv (it may carry a `duck://` token).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub detail: String,
}

impl Refusal {
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Refusal {
            reason,
            detail: detail.into(),
        }
    }

    /// An I/O error at a named path.
    pub fn io(reason: &'static str, path: &std::path::Path, error: &std::io::Error) -> Self {
        Refusal::new(reason, format!("{}: {error}", path.display()))
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.detail, self.reason)
    }
}

impl std::error::Error for Refusal {}
