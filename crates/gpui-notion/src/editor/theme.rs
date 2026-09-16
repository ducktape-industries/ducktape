//! The editor's own token layer: every size, rhythm and colour the document
//! is drawn with, in one place an application can change.
//!
//! Nothing here is a literal at a call site. The metrics are multiples of the
//! active theme's base font size — so the whole document zooms as one system
//! when `Theme::font_size` changes, the way a `rem` scale does on the web —
//! and the colours are either roles read straight from `gpui-kit` or, where
//! the editor invents something the framework has no name for (the wash
//! behind commented text, the palette a highlight mark picks from), fields an
//! application can set.
//!
//! ```ignore
//! EditorTheme::customize(cx, |theme, _| {
//!     theme.page_width = theme.rem * 52.;
//!     theme.comment_fill = theme.accent_wash;
//! });
//! ```
//!
//! The customization is re-applied whenever the `gpui-kit` theme changes, so
//! it survives a switch to dark and a change of base size.

use std::rc::Rc;

use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::{App, FontWeight, Global, Hsla, Pixels, px};

use super::mark::{HighlightColor, TextColor};

/// How a heading of one level is set: its size as a multiple of the base, its
/// weight, and the space above it as a multiple of its *own* size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadingStyle {
    pub size: Pixels,
    pub weight: FontWeight,
    pub line_height: f32,
    /// Space above, in multiples of this heading's own size — the template's
    /// `em`-relative margins.
    pub margin_above: f32,
}

/// The nine text colours a `textStyle` mark can carry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextPalette {
    pub gray: Hsla,
    pub brown: Hsla,
    pub orange: Hsla,
    pub yellow: Hsla,
    pub green: Hsla,
    pub blue: Hsla,
    pub purple: Hsla,
    pub pink: Hsla,
    pub red: Hsla,
}

/// The seven washes a `highlight` mark can carry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HighlightPalette {
    pub yellow: Hsla,
    pub green: Hsla,
    pub blue: Hsla,
    pub purple: Hsla,
    pub pink: Hsla,
    pub red: Hsla,
    pub gray: Hsla,
}

/// Everything the editor draws itself with.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorTheme {
    /// The base size the rest is a multiple of: the theme's font size, which
    /// is also the window's `rem`.
    pub rem: Pixels,

    // ------------------------------------------------------------ typography
    /// Body text.
    pub text_size: Pixels,
    /// Body leading, as a multiple of the text size.
    pub line_height: f32,
    /// Headings one to four; anything deeper uses the last.
    pub headings: [HeadingStyle; 4],
    /// Code blocks and inline code.
    pub code_text_size: Pixels,
    pub code_line_height: f32,
    /// Table cells, a step down from the body.
    pub table_text_size: Pixels,
    pub table_line_height: f32,
    /// Labels inside menus, the toolbar and other chrome.
    pub ui_text_size: Pixels,
    /// The smallest chrome text: a language name, a toggle's caret label.
    pub ui_small_text_size: Pixels,

    // ------------------------------------------------------------------ page
    /// Width of the content column before it starts shrinking with the window.
    pub page_width: Pixels,
    /// Space inside the column, left and right.
    pub page_padding: Pixels,
    /// Space under the last block, clickable to append a paragraph.
    pub page_bottom: Pixels,
    /// Reach either side of the column, where the handles live.
    pub gutter_width: Pixels,
    /// Width of the `+` and drag-handle pair.
    pub gutter_controls_width: Pixels,

    // ---------------------------------------------------------------- rhythm
    /// Space above a block that follows another.
    pub block_gap: Pixels,
    /// Space above a block that opens a new section (a list, a quote, a table).
    pub section_gap: Pixels,
    /// Indent per nesting level.
    pub indent_width: Pixels,
    /// Width reserved for a bullet, number or checkbox.
    pub marker_width: Pixels,
    /// Padding inside a block's own surface — a code block, a callout.
    pub block_padding: Pixels,

    // -------------------------------------------------------------- surfaces
    /// Corner of a small control: a checkbox, a chip.
    pub radius_sm: Pixels,
    /// Corner of a block surface: a code block, a callout, an image.
    pub radius: Pixels,
    /// Corner of a floating surface: the slash menu, the toolbar.
    pub radius_lg: Pixels,

    // ----------------------------------------------------------------- input
    /// What an input is assumed to keep for itself before it has laid out and
    /// said otherwise. See [`super::fit::InputFit`].
    pub input_pad_x: Pixels,
    pub input_pad_y: Pixels,
    /// The most of a block an input may claim before the measurement is
    /// treated as nonsense and ignored.
    pub max_input_inset: Pixels,

    // --------------------------------------------------------------- colours
    /// Fill behind an inline `code` mark.
    pub code_background: Hsla,
    /// Text of an inline `code` mark.
    pub code_foreground: Hsla,
    /// Surface of a code block.
    pub code_block_background: Hsla,
    /// The bar down the left of a quote.
    pub quote_bar: Hsla,
    /// Surface of a callout.
    pub callout_background: Hsla,
    /// Header row of a table.
    pub table_header_background: Hsla,
    /// The wash behind commented text.
    pub comment_fill: Hsla,
    /// The line under commented text, and beside a quoted thread.
    pub comment_accent: Hsla,
    /// Text colours a `textStyle` mark picks from.
    pub text_palette: TextPalette,
    /// Washes a `highlight` mark picks from.
    pub highlight_palette: HighlightPalette,
}

impl EditorTheme {
    /// Build the editor's tokens from the active `gpui-kit` theme.
    pub fn derive(theme: &Theme) -> Self {
        let rem = theme.font_size;
        let dark = theme.mode.is_dark();
        let rems = |n: f32| rem * n;

        Self {
            rem,

            text_size: rem,
            line_height: 1.6,
            headings: [
                HeadingStyle {
                    size: rems(1.5),
                    weight: FontWeight::BOLD,
                    line_height: 1.3,
                    margin_above: 3.0,
                },
                HeadingStyle {
                    size: rems(1.25),
                    weight: FontWeight::BOLD,
                    line_height: 1.3,
                    margin_above: 2.5,
                },
                HeadingStyle {
                    size: rems(1.125),
                    weight: FontWeight::SEMIBOLD,
                    line_height: 1.3,
                    margin_above: 2.0,
                },
                HeadingStyle {
                    size: rem,
                    weight: FontWeight::SEMIBOLD,
                    line_height: 1.3,
                    margin_above: 2.0,
                },
            ],
            code_text_size: rems(0.875),
            code_line_height: 1.5,
            table_text_size: rems(0.9375),
            table_line_height: 1.5,
            ui_text_size: rems(0.875),
            ui_small_text_size: rems(0.6875),

            page_width: rems(44.25),
            page_padding: rems(3.),
            page_bottom: rems(17.5),
            gutter_width: rems(6.),
            gutter_controls_width: rems(3.25),

            block_gap: rems(1.25),
            section_gap: rems(1.5),
            indent_width: rems(1.5),
            marker_width: rems(1.5),
            block_padding: rems(1.),

            radius_sm: theme.radius / 2.,
            radius: theme.radius,
            radius_lg: theme.radius_lg,

            input_pad_x: rems(0.625),
            input_pad_y: rems(0.5),
            max_input_inset: rems(4.),

            code_background: {
                let mut color = theme.muted;
                color.a = if dark { 0.55 } else { 0.9 };
                color
            },
            code_foreground: hsl_of(if dark { 0xff8a80 } else { 0xeb5757 }),
            code_block_background: theme.muted.opacity(if dark { 0.45 } else { 0.7 }),
            quote_bar: theme.border,
            callout_background: theme.muted.opacity(if dark { 0.45 } else { 0.7 }),
            table_header_background: theme.muted.opacity(if dark { 0.35 } else { 0.6 }),
            comment_fill: hsl_of(if dark { 0x4a3a12 } else { 0xfdf0d5 }),
            comment_accent: hsl_of(if dark { 0xd9a441 } else { 0xd9880f }),
            text_palette: TextPalette {
                gray: hsl_of(if dark { 0x9b9a97 } else { 0x787774 }),
                brown: hsl_of(if dark { 0xba856f } else { 0x9f6b53 }),
                orange: hsl_of(if dark { 0xc77d48 } else { 0xd9730d }),
                yellow: hsl_of(if dark { 0xca9849 } else { 0xcb912f }),
                green: hsl_of(if dark { 0x529e72 } else { 0x448361 }),
                blue: hsl_of(if dark { 0x5e87c9 } else { 0x337ea9 }),
                purple: hsl_of(if dark { 0x9d68d3 } else { 0x9065b0 }),
                pink: hsl_of(if dark { 0xd15796 } else { 0xc14c8a }),
                red: hsl_of(if dark { 0xdf5452 } else { 0xd44c47 }),
            },
            highlight_palette: HighlightPalette {
                yellow: hsl_of(if dark { 0x5c4d1a } else { 0xfef3c0 }),
                green: hsl_of(if dark { 0x1f4a33 } else { 0xd3f0e0 }),
                blue: hsl_of(if dark { 0x1e3a5c } else { 0xd3e5ef }),
                purple: hsl_of(if dark { 0x3f2b56 } else { 0xe8deee }),
                pink: hsl_of(if dark { 0x522038 } else { 0xf5e0e9 }),
                red: hsl_of(if dark { 0x5c2323 } else { 0xfbe4e4 }),
                gray: hsl_of(if dark { 0x3a3a3a } else { 0xe3e2e0 }),
            },
        }
    }

    /// `n` times the base size — the editor's `rem`.
    pub fn rems(&self, n: f32) -> Pixels {
        self.rem * n
    }

    /// How a heading of `level` is set. Levels below one and above the table
    /// clamp to its ends.
    pub fn heading(&self, level: u8) -> HeadingStyle {
        let ix = (level.max(1) as usize - 1).min(self.headings.len() - 1);
        self.headings[ix]
    }

    /// The wash a highlight mark of this colour paints.
    pub fn highlight_fill(&self, color: Option<HighlightColor>) -> Hsla {
        let palette = &self.highlight_palette;
        match color.unwrap_or(HighlightColor::Yellow) {
            HighlightColor::Yellow => palette.yellow,
            HighlightColor::Green => palette.green,
            HighlightColor::Blue => palette.blue,
            HighlightColor::Purple => palette.purple,
            HighlightColor::Pink => palette.pink,
            HighlightColor::Red => palette.red,
            HighlightColor::Gray => palette.gray,
        }
    }

    /// The colour a text mark paints; `Default` keeps the block's own colour.
    pub fn text_color(&self, color: TextColor) -> Option<Hsla> {
        let palette = &self.text_palette;
        Some(match color {
            TextColor::Default => return None,
            TextColor::Gray => palette.gray,
            TextColor::Brown => palette.brown,
            TextColor::Orange => palette.orange,
            TextColor::Yellow => palette.yellow,
            TextColor::Green => palette.green,
            TextColor::Blue => palette.blue,
            TextColor::Purple => palette.purple,
            TextColor::Pink => palette.pink,
            TextColor::Red => palette.red,
        })
    }

    // ------------------------------------------------------------- the global

    /// Install the tokens, derived from the theme the application has set.
    pub fn init(cx: &mut App) {
        let tokens = Rc::new(EditorTheme::derive(cx.theme()));
        cx.set_global(EditorThemeGlobal {
            tokens,
            customize: None,
        });
    }

    /// The tokens in force.
    pub fn global(cx: &App) -> &EditorTheme {
        &cx.global::<EditorThemeGlobal>().tokens
    }

    /// The tokens in force, to hold on to past the borrow of `cx` — what a
    /// block's render context carries.
    pub fn shared(cx: &App) -> Rc<EditorTheme> {
        cx.global::<EditorThemeGlobal>().tokens.clone()
    }

    /// Change the tokens. The closure is kept and re-applied on top of the
    /// freshly derived values whenever the `gpui-kit` theme changes, so a
    /// customized editor stays customized across light, dark and zoom.
    pub fn customize(cx: &mut App, customize: impl Fn(&mut EditorTheme, &Theme) + 'static) {
        if !cx.has_global::<EditorThemeGlobal>() {
            Self::init(cx);
        }
        let customize = Rc::new(customize);
        cx.global_mut::<EditorThemeGlobal>().customize = Some(customize);
        Self::refresh(cx);
    }

    /// Derive the tokens again, keeping any customization.
    pub fn refresh(cx: &mut App) {
        if !cx.has_global::<EditorThemeGlobal>() {
            return Self::init(cx);
        }
        let mut tokens = EditorTheme::derive(cx.theme());
        let customize = cx.global::<EditorThemeGlobal>().customize.clone();
        if let Some(customize) = customize {
            customize(&mut tokens, cx.theme());
        }
        cx.global_mut::<EditorThemeGlobal>().tokens = Rc::new(tokens);
    }
}

/// Read the editor's tokens the way `cx.theme()` reads the framework's.
pub trait ActiveEditorTheme {
    fn editor_theme(&self) -> &EditorTheme;
}

impl ActiveEditorTheme for App {
    fn editor_theme(&self) -> &EditorTheme {
        EditorTheme::global(self)
    }
}

impl<T: 'static> ActiveEditorTheme for gpui_kit::Context<'_, T> {
    fn editor_theme(&self) -> &EditorTheme {
        EditorTheme::global(self)
    }
}

/// What an application hands [`EditorTheme::customize`]: the freshly derived
/// tokens to change, and the framework theme they came from.
type Customization = Rc<dyn Fn(&mut EditorTheme, &Theme)>;

struct EditorThemeGlobal {
    tokens: Rc<EditorTheme>,
    customize: Option<Customization>,
}

impl Global for EditorThemeGlobal {}

/// A palette value. Raw colours belong in a theme definition, which is what
/// this file is; everywhere else reads them from here by name.
fn hsl_of(rgb: u32) -> Hsla {
    gpui_kit::rgb(rgb).into()
}

/// Slack added when a text area turns out to be shorter than its text.
///
/// This is a device-pixel allowance, not a design value: the text area is
/// snapped to whole device pixels, so a correction of exactly the shortfall
/// can land a hair short again. It does not scale with the base size.
pub const INPUT_INSET_SLACK: Pixels = px(2.);

// ------------------------------------------------------- appearance actions

gpui_kit::actions!(
    notion_appearance,
    [
        /// Switch between the light and dark palettes.
        ToggleAppearance,
        /// Set the document in a larger size.
        ZoomIn,
        /// Set the document in a smaller size.
        ZoomOut,
        /// Put the document back to its default size.
        ZoomReset,
    ]
);

/// Base text sizes the zoom actions step through, in logical pixels.
pub const ZOOM_STEPS: &[f32] = &[13., 14., 16., 18., 20., 24., 28.];

/// The size a document is set in when nothing has changed it.
pub const DEFAULT_TEXT_SIZE: f32 = 16.;

/// Install the appearance actions and their keys.
///
/// Optional: an application that owns its own appearance menu can leave this
/// out and call [`set_base_size`] and `Theme::change` itself. What it cannot
/// leave out is that the editor follows both, which it does by deriving its
/// tokens from the theme.
pub fn init_appearance_actions(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("secondary-shift-l", ToggleAppearance, None),
        gpui_kit::KeyBinding::new("secondary-=", ZoomIn, None),
        gpui_kit::KeyBinding::new("secondary-+", ZoomIn, None),
        gpui_kit::KeyBinding::new("secondary--", ZoomOut, None),
        gpui_kit::KeyBinding::new("secondary-0", ZoomReset, None),
    ]);
    cx.on_action(|_: &ToggleAppearance, cx| toggle_appearance(cx));
    cx.on_action(|_: &ZoomIn, cx| zoom(1, cx));
    cx.on_action(|_: &ZoomOut, cx| zoom(-1, cx));
    cx.on_action(|_: &ZoomReset, cx| set_base_size(DEFAULT_TEXT_SIZE, cx));
}

/// Swap the palette, keeping the size the reader chose.
pub fn toggle_appearance(cx: &mut App) {
    use gpui_kit::component::ThemeMode;
    let next = if cx.theme().mode.is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    let size = cx.theme().font_size;
    Theme::change(next, None, cx);
    cx.global_mut::<Theme>().font_size = size;
    EditorTheme::refresh(cx);
    cx.refresh_windows();
}

/// Set the base size every metric in the document is a multiple of.
pub fn set_base_size(size: f32, cx: &mut App) {
    cx.global_mut::<Theme>().font_size = px(size);
    EditorTheme::refresh(cx);
    cx.refresh_windows();
}

/// Step the base size along [`ZOOM_STEPS`].
pub fn zoom(step: isize, cx: &mut App) {
    let current = f32::from(cx.theme().font_size);
    let nearest = ZOOM_STEPS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - current)
                .abs()
                .total_cmp(&(*b - current).abs())
        })
        .map(|(ix, _)| ix as isize)
        .unwrap_or(2);
    let next = (nearest + step).clamp(0, ZOOM_STEPS.len() as isize - 1) as usize;
    set_base_size(ZOOM_STEPS[next], cx);
}

// ----------------------------------------------------------- template palette

/// The Notion-like template's own palette, as a `gpui-kit` theme set.
const TEMPLATE_THEMES: &str = include_str!("../../assets/themes/notion.json");

/// Set the editor's document colours to the ones the Tiptap Notion-like
/// template uses, for both appearances.
///
/// The palette is installed as the theme's light and dark configurations, so
/// switching appearance keeps it — it is the application's theme from then on,
/// not a coat of paint over the framework's. The mark palettes, which are the
/// editor's own, are set alongside it.
pub fn apply_template_palette(cx: &mut App) -> anyhow::Result<()> {
    use gpui_kit::component::ThemeMode;
    use std::rc::Rc as StdRc;

    let set: gpui_kit::component::ThemeSet = serde_json::from_str(TEMPLATE_THEMES)?;
    let (mut light, mut dark) = (None, None);
    for config in set.themes {
        if config.mode.is_dark() {
            dark = Some(StdRc::new(config));
        } else {
            light = Some(StdRc::new(config));
        }
    }
    let (Some(light), Some(dark)) = (light, dark) else {
        anyhow::bail!("the template palette needs a light and a dark theme");
    };

    let mode = cx.theme().mode;
    {
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
    }
    let size = cx.theme().font_size;
    Theme::change(mode, None, cx);
    cx.global_mut::<Theme>().font_size = size;

    EditorTheme::customize(cx, |theme, kit| {
        let dark = kit.mode == ThemeMode::Dark;
        // Notion's own text and highlight palettes, which the template ships
        // verbatim; see docs/research/tiptap-notion-spec.md §2.
        theme.text_palette = TextPalette {
            gray: hsl_of(if dark { 0x9c9c9c } else { 0x787673 }),
            brown: hsl_of(if dark { 0xb9856e } else { 0x9d6a53 }),
            orange: hsl_of(if dark { 0xd9730d } else { 0xc77d48 }),
            yellow: hsl_of(if dark { 0xca994e } else { 0xca922f }),
            green: hsl_of(if dark { 0x519e71 } else { 0x448361 }),
            blue: hsl_of(if dark { 0x3699d3 } else { 0x327da9 }),
            purple: hsl_of(if dark { 0x9e69d3 } else { 0x8f64af }),
            pink: hsl_of(if dark { 0xd15796 } else { 0xc24c8b }),
            red: hsl_of(if dark { 0xdf5553 } else { 0xd34a45 }),
        };
        theme.highlight_palette = HighlightPalette {
            yellow: hsl_of(if dark { 0x6b6524 } else { 0xfef9c3 }),
            green: hsl_of(if dark { 0x509568 } else { 0xdcfce7 }),
            blue: hsl_of(if dark { 0x6e92aa } else { 0xe0f2fe }),
            purple: hsl_of(if dark { 0x583e74 } else { 0xf3e8ff }),
            pink: hsl_of(if dark { 0x4e2c3c } else { 0xfcf1f6 }),
            red: hsl_of(if dark { 0x743e42 } else { 0xffe4e6 }),
            gray: hsl_of(if dark { 0x2f2f2f } else { 0xf8f8f7 }),
        };
        // Inline code and the code block come off the template's gray ramps.
        theme.code_background = hsla_of(if dark { 0xe7e7f3 } else { 0x0f1624 }, if dark { 0.07 } else { 0.05 });
        theme.code_foreground = hsla_of(if dark { 0xfbfbfe } else { 0x23252a }, if dark { 0.75 } else { 0.87 });
        theme.code_block_background = hsla_of(
            if dark { 0xe8e8fd } else { 0x383838 },
            if dark { 0.05 } else { 0.04 },
        );
        theme.callout_background = theme.code_block_background;
        theme.table_header_background = theme.code_block_background;
        theme.quote_bar = kit.foreground.opacity(0.85);
    });
    Ok(())
}

/// A palette value with an alpha, for the template's translucent ramps.
fn hsla_of(rgb: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = gpui_kit::rgb(rgb).into();
    color.a = alpha;
    color
}
