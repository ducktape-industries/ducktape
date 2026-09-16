//! The product's visual system: bundled fonts, the type scale, and the one
//! palette both the native shell (through the kit's theme) and the WASM
//! views (through `ducktape_view_guest::kit`) paint with.

/// Font identity. The native shell loads these files into GPUI's text system.
/// Guest wire text names the same families. Replace an asset and its family
/// constant together when changing the product face.
pub mod fonts {
    /// the UI face — every sans role (default, medium, display).
    pub const FAMILY_UI: &str = "Geist";
    /// the data face — hashes, seqs, diffs, code, the log ring.
    pub const FAMILY_MONO: &str = "Geist Mono";
    /// Bundled files relative to this crate. The emoji face supplies fallback
    /// glyphs; it is not a separate product type role. ONE FILE PER WEIGHT:
    /// the shell's text system draws a whole family at the weight of the face
    /// it matched, so a family holding one variable face has one weight. Two
    /// weights, not four — a heavier request lands on the nearer of these
    /// (see `app/src/shell.rs`'s `LATIN_FACES`).
    pub const ASSETS: [&str; 5] = [
        "assets/fonts/Geist-Regular.ttf",
        "assets/fonts/Geist-Bold.ttf",
        "assets/fonts/GeistMono-Regular.ttf",
        "assets/fonts/GeistMono-Bold.ttf",
        "assets/fonts/NotoColorEmoji.ttf",
    ];
    /// The variable faces [`ASSETS`]' Latin instances are cut from, by
    /// `ops/build-font-instances.sh`. Kept so a regeneration needs nothing
    /// off the network; nothing registers them.
    pub const SOURCES: [&str; 2] = [
        "assets/fonts/Geist[wght].ttf",
        "assets/fonts/GeistMono[wght].ttf",
    ];
}

/// Text sizes, in pixels. One dense scale for the shell and every view: the
/// body is 13px and nothing in the chrome is louder than 16px.
pub mod type_scale {
    /// a page title — a view's own title row
    pub const TITLE: f64 = 16.;
    /// a section title inside a view
    pub const SECTION: f64 = 13.5;
    /// The native shell's default text size.
    pub const BODY: f64 = 13.;
    /// secondary copy beside body text
    pub const SECONDARY: f64 = 12.;
    /// a caption, a timestamp, a badge
    pub const CAPTION: f64 = 11.;
    /// identifiers in the data face
    pub const MONO: f64 = 12.;
}

/// Corner radii, in pixels. Tight: a control is barely rounded, a card a
/// touch more, and only an avatar is a circle.
pub mod radius {
    /// a control: a button, an input, a list row
    pub const CONTROL: f64 = 4.;
    /// a card, a panel, a modal
    pub const CARD: f64 = 6.;
    /// a pill: an avatar
    pub const PILL: f64 = 999.;
}

/// One sRGB color as the wire carries it: `[r, g, b, a]` in `0.0..=1.0`.
pub type Color = [f32; 4];

/// The named colors of one appearance. Cool neutral greys, an ink sidebar in
/// both modes, one indigo accent for what is live or chosen, and the four
/// status tones.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// the sidebar rail: ink in both modes
    pub sidebar: Color,
    /// text on the sidebar
    pub sidebar_foreground: Color,
    /// secondary text on the sidebar: section names, the resting rows
    pub sidebar_muted: Color,
    /// the chosen or hovered sidebar row
    pub sidebar_raised: Color,
    /// hairlines on the sidebar
    pub sidebar_border: Color,
    /// the window
    pub background: Color,
    /// a sidebar, a pane, a card — one step off the window
    pub surface: Color,
    /// a raised surface: a hovered row, a code block
    pub surface_raised: Color,
    /// hairlines between regions
    pub border: Color,
    /// the border of a control
    pub border_strong: Color,
    /// body text
    pub foreground: Color,
    /// secondary text
    pub muted: Color,
    /// faint text: placeholders, disabled
    pub faint: Color,
    /// the accent itself: the live dot, the focus ring, the selection bar
    pub accent: Color,
    /// the accent as a wash behind a chosen row
    pub accent_soft: Color,
    /// text on the accent wash
    pub accent_foreground: Color,
    /// a primary action's fill (ink) and its text
    pub primary: Color,
    pub primary_foreground: Color,
    pub link: Color,
    pub success: Color,
    pub success_soft: Color,
    pub warning: Color,
    pub warning_soft: Color,
    pub danger: Color,
    pub danger_soft: Color,
    /// an agent's identity tint
    pub agent: Color,
    pub agent_soft: Color,
}

const fn hex(value: u32) -> Color {
    [
        ((value >> 16) & 0xff) as f32 / 255.,
        ((value >> 8) & 0xff) as f32 / 255.,
        (value & 0xff) as f32 / 255.,
        1.,
    ]
}

pub const LIGHT: Palette = Palette {
    sidebar: hex(0x17181C),
    sidebar_foreground: hex(0xE8E8EC),
    sidebar_muted: hex(0x8B8D97),
    sidebar_raised: hex(0x26282F),
    sidebar_border: hex(0x2A2C33),
    background: hex(0xFFFFFF),
    surface: hex(0xF7F7F8),
    surface_raised: hex(0xEDEDF0),
    border: hex(0xE4E4E8),
    border_strong: hex(0xD0D0D6),
    foreground: hex(0x1B1B1F),
    muted: hex(0x6B6C76),
    faint: hex(0xA2A3AC),
    accent: hex(0x5B5FC7),
    accent_soft: hex(0xECEDFB),
    accent_foreground: hex(0x3B3FA8),
    primary: hex(0x1B1B1F),
    primary_foreground: hex(0xFFFFFF),
    link: hex(0x4C52C9),
    success: hex(0x1F9D55),
    success_soft: hex(0xE3F5EA),
    warning: hex(0xB4700F),
    warning_soft: hex(0xFBF0DA),
    danger: hex(0xD33A2E),
    danger_soft: hex(0xFCE6E4),
    agent: hex(0x7A4BD8),
    agent_soft: hex(0xF0EAFC),
};

pub const DARK: Palette = Palette {
    sidebar: hex(0x101114),
    sidebar_foreground: hex(0xE7E7EA),
    sidebar_muted: hex(0x7E8189),
    sidebar_raised: hex(0x1E2026),
    sidebar_border: hex(0x24262C),
    background: hex(0x151619),
    surface: hex(0x1B1C20),
    surface_raised: hex(0x24262C),
    border: hex(0x27292F),
    border_strong: hex(0x363940),
    foreground: hex(0xE7E7EA),
    muted: hex(0x8E9098),
    faint: hex(0x5E616A),
    accent: hex(0x7C82E8),
    accent_soft: hex(0x24263D),
    accent_foreground: hex(0xB4B8F5),
    primary: hex(0xE7E7EA),
    primary_foreground: hex(0x151619),
    link: hex(0x8F95F0),
    success: hex(0x4CC27E),
    success_soft: hex(0x16301F),
    warning: hex(0xE1A93F),
    warning_soft: hex(0x3A2C10),
    danger: hex(0xF0665A),
    danger_soft: hex(0x3E1B14),
    agent: hex(0xA78BF5),
    agent_soft: hex(0x2A2340),
};

/// The palette of an appearance.
pub const fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// `#rrggbb` (or `#rrggbbaa` when translucent) — the notation the kit's
/// theme JSON and SVG both read.
pub fn css(color: Color) -> String {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    let [r, g, b, a] = color;
    if a >= 1. {
        format!("#{:02x}{:02x}{:02x}", channel(r), channel(g), channel(b))
    } else {
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            channel(r),
            channel(g),
            channel(b),
            channel(a)
        )
    }
}

/// The same palette as a gpui-kit theme set: two themes, one per mode, so
/// the kit's own controls (buttons, inputs, checkboxes, scrollbars) paint
/// with the colors the views paint with.
pub fn kit_theme_json() -> String {
    let theme = |name: &str, mode: &str, p: &Palette| {
        let c = css;
        format!(
            r##"{{
  "name": "{name}",
  "mode": "{mode}",
  "font.family": "{ui}",
  "font.size": {body},
  "mono_font.family": "{mono}",
  "mono_font.size": {mono_size},
  "radius": {radius},
  "radius.lg": {radius_lg},
  "shadow": false,
  "colors": {{
    "background": "{bg}",
    "foreground": "{fg}",
    "border": "{border}",
    "input.border": "{border_strong}",
    "ring": "{accent}",
    "caret": "{fg}",
    "selection.background": "{selection}",
    "muted.background": "{surface}",
    "muted.foreground": "{muted}",
    "accent.background": "{surface_raised}",
    "accent.foreground": "{fg}",
    "secondary.background": "{surface}",
    "secondary.hover.background": "{surface_raised}",
    "secondary.active.background": "{accent_soft}",
    "secondary.foreground": "{fg}",
    "primary.background": "{primary}",
    "primary.hover.background": "{primary}",
    "primary.active.background": "{primary}",
    "primary.foreground": "{primary_fg}",
    "danger.background": "{danger}",
    "danger.foreground": "{bg}",
    "success.background": "{success}",
    "success.foreground": "{bg}",
    "warning.background": "{warning}",
    "warning.foreground": "{bg}",
    "info.background": "{link}",
    "info.foreground": "{bg}",
    "link.foreground": "{link}",
    "link.hover.foreground": "{link}",
    "link.active.foreground": "{link}",
    "popover.background": "{bg}",
    "popover.foreground": "{fg}",
    "list.background": "{bg}",
    "list.hover.background": "{surface}",
    "list.active.background": "{accent_soft}",
    "list.active.border": "{accent}",
    "list.even.background": "{bg}",
    "list.head.background": "{surface}",
    "table.background": "{bg}",
    "table.hover.background": "{surface}",
    "table.active.background": "{accent_soft}",
    "table.active.border": "{accent}",
    "table.even.background": "{bg}",
    "table.head.background": "{surface}",
    "table.head.foreground": "{muted}",
    "table.row.border": "{border}",
    "sidebar.background": "{sidebar}",
    "sidebar.foreground": "{sidebar_fg}",
    "sidebar.border": "{sidebar_border}",
    "sidebar.accent.background": "{sidebar_raised}",
    "sidebar.accent.foreground": "{sidebar_fg}",
    "sidebar.primary.background": "{accent}",
    "sidebar.primary.foreground": "{primary_fg}",
    "tab_bar.background": "{surface}",
    "tab_bar.segmented.background": "{surface}",
    "tab.background": "{surface}",
    "tab.foreground": "{muted}",
    "tab.active.background": "{bg}",
    "tab.active.foreground": "{fg}",
    "group_box.background": "{surface}",
    "group_box.foreground": "{fg}",
    "description_list_label.background": "{surface}",
    "description_list_label.foreground": "{muted}",
    "switch.background": "{border_strong}",
    "slider.bar.background": "{accent}",
    "slider.thumb.background": "{bg}",
    "progress_bar.background": "{accent}",
    "skeleton.background": "{surface_raised}",
    "scrollbar.background": "{bg}00",
    "scrollbar.thumb.background": "{border_strong}",
    "scrollbar.thumb.hover.background": "{muted}",
    "drag_border": "{accent}",
    "drop_target.background": "{accent_soft}",
    "title_bar.background": "{bg}",
    "title_bar.border": "{border}",
    "window.border": "{border}"
  }}
}}"##,
            ui = fonts::FAMILY_UI,
            mono = fonts::FAMILY_MONO,
            body = type_scale::BODY,
            mono_size = type_scale::MONO,
            radius = radius::CONTROL as usize,
            radius_lg = radius::CARD as usize,
            sidebar = c(p.sidebar),
            sidebar_fg = c(p.sidebar_foreground),
            sidebar_border = c(p.sidebar_border),
            sidebar_raised = c(p.sidebar_raised),
            bg = c(p.background),
            fg = c(p.foreground),
            border = c(p.border),
            border_strong = c(p.border_strong),
            accent = c(p.accent),
            selection = c([p.accent[0], p.accent[1], p.accent[2], 0.35]),
            surface = c(p.surface),
            surface_raised = c(p.surface_raised),
            muted = c(p.muted),
            accent_soft = c(p.accent_soft),
            primary = c(p.primary),
            primary_fg = c(p.primary_foreground),
            danger = c(p.danger),
            success = c(p.success),
            warning = c(p.warning),
            link = c(p.link),
        )
    };
    format!(
        "{{\"name\": \"Ducktape\", \"themes\": [{}, {}]}}",
        theme(LIGHT_THEME, "light", &LIGHT),
        theme(DARK_THEME, "dark", &DARK)
    )
}

/// The theme names `kit_theme_json` registers.
pub const LIGHT_THEME: &str = "Ducktape Light";
pub const DARK_THEME: &str = "Ducktape Dark";

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_embedded_font_file_exists_and_is_truetype() {
        for asset in fonts::ASSETS.iter().chain(&fonts::SOURCES) {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(asset);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("font asset {asset} unreadable: {error}"));
            let magic = &bytes[..4];
            assert!(
                magic == b"\x00\x01\x00\x00" || magic == b"OTTO" || magic == b"true",
                "{asset} is not a TrueType/OpenType file"
            );
        }
    }

    #[test]
    fn css_notation_round_trips_the_palette() {
        assert_eq!(css(hex(0x5B5FC7)), "#5b5fc7");
        assert_eq!(css([1., 1., 1., 0.5]), "#ffffff80");
        assert_eq!(css(LIGHT.background), "#ffffff");
        assert_eq!(css(DARK.background), "#151619");
        assert_eq!(css(LIGHT.sidebar), "#17181c");
    }

    #[test]
    fn the_kit_theme_json_names_both_modes_and_the_product_fonts() {
        let json = kit_theme_json();
        assert!(json.contains("\"Ducktape Light\""));
        assert!(json.contains("\"Ducktape Dark\""));
        assert!(json.contains("\"font.family\": \"Geist\""));
        assert!(json.contains("\"mode\": \"dark\""));
        let braces = json.matches('{').count();
        assert_eq!(braces, json.matches('}').count());
    }
}
