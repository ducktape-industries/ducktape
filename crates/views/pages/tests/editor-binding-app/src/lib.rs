#[path = "../../../src/editor_indent.rs"]
pub mod indent;
#[path = "../../../src/editor_inline.rs"]
pub mod inline;
#[path = "../../../src/editor_markdown.rs"]
pub mod markdown;
#[path = "../../../src/editor_presentation.rs"]
pub mod presentation;
// Compile the production adapter and reducer, not copies of their behavior.
#[path = "../../../src/document_sync.rs"]
pub mod document_sync;
#[path = "../../../src/editor.rs"]
pub mod editor;
#[path = "../../../src/editor_binding.rs"]
pub mod editor_binding;
#[path = "../../../src/editor_format.rs"]
pub mod format;
#[path = "../../../src/editor_menu.rs"]
pub mod editor_menu;
#[path = "../../../src/editor_view.rs"]
pub mod editor_view;
pub mod fixture_source;
pub mod fixture {
    use crate::fixture_source::DocumentSource;
    use ducktape_view_guest::wire;
    pub fn large_source() -> DocumentSource {
        DocumentSource {
            reference: wire::encode(&crate::fixture_source::DocumentIdentity {
                document: "network-a/page-a/source-1".into(),
                reset: 1,
            }),
        }
    }
}
include!("ui/view.rs");
ducktape_view_guest::export_app!(
    PagesEditorFixture,
    "Pages editor binding",
    "Pages editor transaction regression fixture",
    ["pages"]
);
