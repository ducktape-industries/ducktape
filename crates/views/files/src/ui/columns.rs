//! The directory as Miller columns.

use super::*;

impl FilesView {
    pub(super) fn columns_pane(&self, key: String) -> wire::Node {
        self.list_pane(key)
    }
}
