use super::*;
use ducktape_view_guest::{kit::Tone, slots};

fn span(
    content: String,
    link: Option<String>,
    weight: wire::Weight,
    italic: bool,
) -> wire::RichSpan {
    let decorated = weight != wire::Weight::Normal || italic;
    let linked = link.is_some();
    wire::RichSpan {
        content,
        underline: linked,
        link,
        color: linked.then(|| native::rgba(native::palette().link)),
        font: decorated.then_some(wire::NamedFont {
            family: wire::FontFamily::SansSerif,
            weight,
            stretch: wire::FontStretch::Normal,
            style: if italic {
                wire::FontStyle::Italic
            } else {
                wire::FontStyle::Normal
            },
        }),
        ..Default::default()
    }
}

/// The tone an item or review state paints. Open is live, merged wears the
/// agent tint, and a closed item is spent — only a refused review is danger.
pub(super) fn state_tone(state: &str) -> Tone {
    match state {
        "open" | "approve" | "approved" => Tone::Success,
        "merged" => Tone::Agent,
        "request_changes" | "changes_requested" => Tone::Danger,
        _ => Tone::Neutral,
    }
}

/// A reading's body size: one step over the 13px chrome, at 1.5 leading.
pub(super) const READING: f32 = 14.;

impl ForgeView {
    /// The one strip every refusal lands on. The readers already phrase each
    /// error as a sentence with its verb, so the strip carries nothing else.
    pub(super) fn unavailable(&self, key: String) -> wire::Node {
        native::notice(
            &key,
            native::wrapping(native::text(
                format!("{key}/error"),
                self.host_error.clone(),
            )),
            Tone::Danger,
        )
    }
    pub(super) fn disconnected(&self, key: String) -> wire::Node {
        native::empty_state(
            &key,
            "Not connected",
            "Choose a network from the sidebar to read its repositories.",
        )
    }

    pub(super) fn loading_tracker(&self, key: String) -> wire::Node {
        native::secondary(key, "Loading repository tracker…")
    }
    pub(super) fn tracker_unavailable(&self, key: String) -> wire::Node {
        native::wrapping(native::secondary(
            key,
            "Could not load this repository. Return to all repos and open it again to retry.",
        ))
    }
    pub(super) fn empty_issues(&self, key: String) -> wire::Node {
        native::empty_state(
            format!("{key}/state"),
            "No issues",
            "Open the first one above.",
        )
    }
    pub(super) fn empty_pulls(&self, key: String) -> wire::Node {
        native::empty_state(
            format!("{key}/state"),
            "No pull requests",
            "An agent run opens one when it delivers its work.",
        )
    }
    pub(super) fn loading_item(&self, key: String) -> wire::Node {
        native::secondary(key, "Loading tracker item…")
    }
    pub(super) fn item_unavailable(&self, key: String) -> wire::Node {
        native::wrapping(native::secondary(
            key,
            "Could not load this item. Go back and open it again to retry.",
        ))
    }

    /// A review the chain holds is final; its `created_at` is consensus
    /// time in a unit only the network knows, so no number is shown.
    pub(super) fn finality(&self, key: String) -> wire::Node {
        native::badge(key, "Finalized", Tone::Success)
    }

    fn rich_line(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
        block: &crate::host::ChatBlock,
        size: f32,
    ) -> wire::Node {
        let mut spans = Vec::new();
        for part in &block.spans {
            for next in [
                span(
                    part.mention.clone(),
                    Some(part.mention_link.clone()),
                    wire::Weight::Medium,
                    false,
                ),
                span(
                    part.link_text.clone(),
                    Some(part.link.clone()),
                    wire::Weight::Medium,
                    false,
                ),
                span(part.bold_italic.clone(), None, wire::Weight::Bold, true),
                span(part.bold.clone(), None, wire::Weight::Bold, false),
                span(part.italic.clone(), None, wire::Weight::Normal, true),
                span(part.plain.clone(), None, wire::Weight::Normal, false),
            ] {
                if !next.content.is_empty() {
                    spans.push(next);
                }
            }
        }
        wire::Node::RichText {
            key,
            spans,
            options: wire::TextOptions {
                wrapping: Some(wire::Wrapping::WordOrGlyph),
                line_height: Some(wire::LineHeight::Relative(1.5)),
                ..Default::default()
            },
            size: Some(size),
            color: Some(native::rgba(native::palette().foreground)),
            font: Default::default(),
            width: Some(wire::Length::Fill),
            align_x: None,
            on_link: Some(slots::handler::<String, Message>(Box::new(move |link| {
                Some(open(link))
            }))),
        }
    }

    pub(super) fn rich_body(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
        blocks: Vec<crate::host::ChatBlock>,
    ) -> wire::Node {
        self.rich_blocks(key, open, blocks, native::type_scale::BODY as f32)
    }

    fn rich_blocks(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
        blocks: Vec<crate::host::ChatBlock>,
        size: f32,
    ) -> wire::Node {
        let p = native::palette();
        let mut children = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let key = format!("{key}/block/{index}");
            match block.kind.as_str() {
                "divider" => children.push(native::divider(key)),
                "code" => {
                    let code =
                        native::wrapping(native::mono(format!("{key}/code"), block.text.clone()));
                    let mut lines = Vec::new();
                    if !block.lang.is_empty() {
                        lines.push(native::caption(
                            format!("{key}/language"),
                            block.lang.clone(),
                        ));
                    }
                    lines.push(code);
                    let mut boxed = native::padded(
                        native::spaced(native::column(&key, lines), 4.),
                        wire::Edges::all(10.),
                    );
                    if let wire::Node::Linear {
                        background, border, ..
                    } = &mut boxed
                    {
                        *background = Some(native::rgba(p.surface_raised));
                        *border = Some(wire::Border {
                            color: Some(native::rgba(p.border)),
                            width: Some(1.),
                            radius: Some([native::radius::CONTROL as f32; 4]),
                        });
                    }
                    children.push(boxed);
                }
                "quote" | "paragraph" => {
                    let content = if block.rich {
                        self.rich_line(format!("{key}/text"), open.clone(), block, size)
                    } else {
                        let mut plain = native::wrapping(native::text_size(
                            native::text(format!("{key}/text"), block.text.clone()),
                            size,
                        ));
                        if let wire::Node::Text { options, .. } = &mut plain {
                            options.line_height = Some(wire::LineHeight::Relative(1.5));
                        }
                        plain
                    };
                    let content = if block.kind == "quote" {
                        let mut quote = native::padded(
                            native::row(&key, [content]),
                            wire::Edges {
                                top: 2.,
                                right: 0.,
                                bottom: 2.,
                                left: 12.,
                            },
                        );
                        if let wire::Node::Linear { border, .. } = &mut quote {
                            *border = Some(wire::Border {
                                color: Some(native::rgba(p.border_strong)),
                                width: Some(1.),
                                radius: None,
                            });
                        }
                        quote
                    } else {
                        content
                    };
                    children.push(content);
                }
                _ => {}
            }
        }
        native::spaced(native::column(key, children), 8.)
    }
    /// The item's own body: a reading, wider type at 1.5 leading, held to a
    /// measure a person can track a line across.
    pub(super) fn item_body(
        &self,
        key: String,
        open: impl Fn(String) -> Message + Clone + 'static,
    ) -> wire::Node {
        let mut body = self.rich_blocks(key, open, self.forge_item_blocks.clone(), READING);
        if let wire::Node::Linear { max_width, .. } = &mut body {
            *max_width = Some(720.);
        }
        body
    }
}
