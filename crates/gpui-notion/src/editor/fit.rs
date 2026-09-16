//! Sizing an input to the text in it, by measurement rather than assumption.
//!
//! Every text area in this editor — a block, a table cell — is sized to its
//! own content, because the document, not the input, decides how tall things
//! are. That only works if the height handed to an input leaves room for the
//! text after the input has taken its own padding out of it, and that padding
//! is not a number an application can know: it depends on the component, on
//! the theme and on the display, whose device pixels the text area is snapped
//! to. A text area even a fraction short of its text scrolls inside itself as
//! the caret moves between rows, and the text visibly jumps.
//!
//! So the gap is measured. [`InputFit`] remembers what an input did with the
//! height it was given last frame and asks for more when it came up short.

use gpui_kit::component::input::EditorState;
use gpui_kit::{App, Context, Entity, Pixels, Window, px};

use super::theme::{EditorTheme, INPUT_INSET_SLACK};

/// A text area whose height the document decides.
///
/// Every input in the editor starts here, so none of them can drift into
/// scrolling on its own: no line numbers, no folding, no search, soft wrap
/// on, and — the part that matters — no empty room reserved below the last
/// line. A code editor reserves half a viewport there by default, which makes
/// content taller than the box and puts a scrollbar in a one-line table cell.
pub fn document_text_state(window: &mut Window, cx: &mut Context<EditorState>) -> EditorState {
    EditorState::new(window, cx)
        .line_number(false)
        .folding(false)
        .indent_guides(false)
        .searchable(false)
        .scroll_beyond_last_line(Some(0))
        .soft_wrap(true)
}

/// What one input keeps for itself, learned from what it did last frame.
///
/// `inset` is the height it takes out of the box; `lead` is how far its first
/// glyph sits from the box's own corner. Both are measured: an input pads
/// itself, and a code editor reserves a line-number gutter even with line
/// numbers off, neither of which an application can know.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputFit {
    inset: Pixels,
    lead: gpui_kit::Point<Pixels>,
}

impl InputFit {
    /// What to assume before an input has laid out and said otherwise.
    pub fn new(theme: &EditorTheme) -> Self {
        Self {
            inset: theme.input_pad_y * 2.,
            lead: gpui_kit::point(theme.input_pad_x, theme.input_pad_y),
        }
    }

    /// The height to hand an input that has to show `text` pixels of text.
    pub fn height(&self, text: Pixels) -> Pixels {
        text + self.inset
    }

    /// The height this input takes out of the box it is given.
    pub fn inset(&self) -> Pixels {
        self.inset
    }

    /// How far to pull an input's box back so its first glyph lands on the
    /// spot the layout gave the block.
    pub fn lead(&self) -> gpui_kit::Point<Pixels> {
        self.lead
    }

    /// Learn where the input actually put its first glyph: `slot` is the
    /// corner the layout handed the block, `glyph` is where the text starts.
    ///
    /// `device_pixel` is what one physical pixel is worth in logical ones,
    /// and it is the resolution of the answer: text is painted on the
    /// physical grid, so moving a box by less than one physical pixel can
    /// move the glyph by a whole one, or by none at all. Correcting an error
    /// smaller than that does not remove it — it trades it for another one,
    /// and the text walks back and forth for as long as the window is open.
    /// So the fit only spends an error it can actually pay with.
    pub fn observe_lead(
        &mut self,
        slot: gpui_kit::Point<Pixels>,
        glyph: gpui_kit::Point<Pixels>,
        theme: &EditorTheme,
        device_pixel: Pixels,
    ) {
        let error = glyph - slot;
        let worth_moving = device_pixel.max(px(0.05));
        if error.x.abs() >= worth_moving {
            self.lead.x = (self.lead.x + error.x).clamp(px(0.), theme.max_input_inset);
        }
        if error.y.abs() >= worth_moving {
            self.lead.y = (self.lead.y + error.y).clamp(px(0.), theme.max_input_inset);
        }
    }

    /// Learn from the text area an input ended up with.
    ///
    /// The inset only ever widens: text areas are snapped to whole device
    /// pixels, and following that snapping in both directions never settles.
    /// A text area with far more room than its text asks for means the block
    /// changed shape underneath, and starts the measurement again.
    pub fn observe(&mut self, text: Pixels, text_area: Pixels, theme: &EditorTheme) {
        if text_area <= px(0.) {
            return;
        }
        if text_area < text {
            // A device pixel of slack on top of the shortfall, so the next
            // frame lands over the line rather than on it.
            let widened = self.inset + (text - text_area) + INPUT_INSET_SLACK;
            self.inset = widened.min(theme.max_input_inset);
        } else if text_area - text > INPUT_INSET_SLACK * 4. {
            *self = Self::new(theme);
        }
    }

    /// Whether an input's text area currently holds all of its text, which is
    /// the invariant that keeps a document still.
    pub fn fits(text: Pixels, text_area: Option<Pixels>) -> bool {
        match text_area {
            Some(area) => area + px(0.01) >= text,
            None => true,
        }
    }
}

/// Put an input back to the top when its text area holds all of its text.
///
/// An input scrolls itself to keep the caret in view. While text is being
/// typed a text area can be a row short for one frame — long enough for the
/// input to scroll — and nothing puts that scroll back once the area has
/// caught up. Leaving it there hides the first row of the block.
pub fn reset_scroll_when_text_fits(
    state: &Entity<EditorState>,
    text: Pixels,
    cx: &mut App,
) {
    let (area, offset) = {
        let state = state.read(cx);
        (
            state.text_bounds().map(|bounds| bounds.size.height),
            state.scroll_offset(),
        )
    };
    if offset.y == px(0.) || !InputFit::fits(text, area) {
        return;
    }
    state.update(cx, |state, cx| {
        state.set_scroll_offset(gpui_kit::point(offset.x, px(0.)), cx)
    });
}

/// Height of the text in a single-input text area, in pixels, as the input
/// itself lays it out: at least one row, more when it wraps.
pub fn text_height(
    state: &Entity<EditorState>,
    text: &str,
    fallback_line_height: Pixels,
    cx: &App,
) -> Pixels {
    let state = state.read(cx);
    let line_height = state.line_height().unwrap_or(fallback_line_height);
    state
        .range_to_bounds(&(0..text.len()))
        .map(|bounds| bounds.size.height)
        .unwrap_or(line_height)
        .max(line_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::point;

    fn tokens() -> EditorTheme {
        EditorTheme::derive(&gpui_kit::component::Theme::default())
    }

    /// A display whose physical pixels are 0.9375 of a logical one, which is
    /// what a 1.0667 scale factor gives.
    const DEVICE_PIXEL: Pixels = px(0.9375);

    #[test]
    fn an_error_worth_more_than_a_pixel_is_corrected() {
        let theme = tokens();
        let mut fit = InputFit::new(&theme);
        let before = fit.lead().x;

        // The glyph landed ten pixels right of where the block put the box.
        fit.observe_lead(
            point(px(100.), px(50.)),
            point(px(110.), px(50.)),
            &theme,
            DEVICE_PIXEL,
        );

        assert_eq!(fit.lead().x, before + px(10.));
    }

    #[test]
    fn an_error_smaller_than_a_physical_pixel_is_left_alone() {
        let theme = tokens();
        let mut fit = InputFit::new(&theme);
        let settled = fit.lead();

        // Text is painted on the physical grid, so a fraction of a pixel is
        // not an error the layout can pay off: acting on it moves the glyph
        // a whole pixel the other way, and the document walks.
        for error in [px(0.3125), px(-0.625), px(0.625), px(-0.3125)] {
            fit.observe_lead(
                point(px(100.), px(50.)),
                point(px(100.) + error, px(50.) + error),
                &theme,
                DEVICE_PIXEL,
            );
            assert_eq!(fit.lead(), settled, "moved for an error of {error:?}");
        }
    }

    #[test]
    fn a_correction_settles_in_one_step_and_stays() {
        let theme = tokens();
        let mut fit = InputFit::new(&theme);
        let slot = point(px(144.), px(48.));

        // First frame: the input's own gutter puts the glyph ten pixels in.
        fit.observe_lead(slot, slot + point(px(10.), px(0.)), &theme, DEVICE_PIXEL);
        let settled = fit.lead();

        // From then on the glyph lands within a physical pixel of the slot,
        // wherever the grid rounds it, and the fit stops moving.
        for residual in [px(0.625), px(-0.3125), px(0.625)] {
            fit.observe_lead(slot, slot + point(residual, px(0.)), &theme, DEVICE_PIXEL);
            assert_eq!(fit.lead(), settled);
        }
    }
}
