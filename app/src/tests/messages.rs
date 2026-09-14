use super::*;

#[test]
fn the_native_message_list_preserves_measurements_by_identity() {
    let native = rust_tokens(include_str!("../view_tree.rs"));
    assert!(
        native.contains("gpui_kit::list("),
        "the host uses native viewport virtualization"
    );
    assert!(
        native.contains(".splice(") && native.contains(".remeasure_items("),
        "row identity updates preserve measurements"
    );
}

#[test]
fn native_rich_text_uses_one_paragraph_with_actionable_links() {
    let native = rust_tokens(include_str!("../view_tree.rs"));
    assert!(
        native.contains("StyledText::new("),
        "one native paragraph owns wrapping and links"
    );
    assert!(
        native.contains(".on_click("),
        "the paragraph's link indices are actionable"
    );
}

#[test]
fn the_mention_plate_leaves_space_before_and_after_the_token() {
    use gpui_kit::{FontWeight, TextRun, font, px};
    let cx = crate::frame_probe::headless_context();
    let blocks = chat::client::paragraph_blocks("before<@3>after");
    let spans = &blocks[0].spans;
    let mut text = String::new();
    let mut runs = Vec::new();
    let mut bounds = Vec::new();
    for span in spans {
        let content = if span.mention.is_empty() {
            &span.plain
        } else {
            &span.mention
        };
        let start = text.len();
        text.push_str(content);
        bounds.push(start..text.len());
        let mut face = font("Geist");
        if !span.mention.is_empty() {
            face.weight = FontWeight::MEDIUM;
        }
        runs.push(TextRun {
            len: content.len(),
            font: face,
            ..Default::default()
        });
    }
    let shaper = gpui_kit::WindowTextSystem::new(cx.text_system().clone());
    let line = shaper.shape_line(text.into(), px(13.5), &runs, None);
    let padding = px(1.);
    let leading = line.x_for_index(bounds[2].start) - padding - line.x_for_index(bounds[0].end);
    let trailing = line.x_for_index(bounds[4].start) - line.x_for_index(bounds[2].end) - padding;
    assert!(
        leading >= px(1.5),
        "leading whitespace remains visible: {leading:?}"
    );
    assert!(
        trailing >= px(1.5),
        "trailing whitespace remains visible: {trailing:?}"
    );
}
