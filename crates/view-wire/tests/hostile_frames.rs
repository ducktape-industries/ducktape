//! Property tests for the wire's hostile-input contract: a random tree either
//! comes back out of `decode` refused for a reason the door actually names,
//! or `sanitize` pulls it inside every bound `sanitize_node` promises; bytes a
//! hostile guest could have written never make `decode` panic; and a
//! hand-crafted length-prefix bomb is refused before it is walked.
//!
//! No new dependency: the generator is a splitmix64 PRNG seeded by a fixed
//! constant, so a failure prints its seed and the run reproduces exactly.

use std::collections::HashSet;

use view_wire::*;

/// Mirrors the wire's own private ceiling on decoded nodes (`16 *
/// MAX_NODES`, see `decode`'s doc comment): decode refuses a frame that
/// would build more than this many nodes, independent of `MAX_NODES` itself,
/// which only bounds what `sanitize` keeps.
const MAX_DECODED_NODES: usize = 16 * MAX_NODES;
/// Mirrors the wire's private `MAX_PIXELS`: every non-text size sanitize
/// keeps is clamped to this range.
const PIXEL_BOUND: f32 = 8192.0;
/// Mirrors the wire's private `MAX_TEXT_PIXELS`: a text size is clamped
/// tighter than any other length, since it drives glyph rasterization.
const TEXT_PIXEL_BOUND: f32 = 512.0;

// ---------------------------------------------------------------- splitmix64

/// A tiny deterministic PRNG so the property tests need no new dependency.
/// splitmix64: https://prng.di.unimi.it/splitmix64.c
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_range(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() as usize) % bound
    }

    fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// A fraction in `0.0..1.0`, from the PRNG's top 24 bits.
    fn next_unit(&mut self) -> f64 {
        ((self.next_u64() >> 40) as f64) / ((1u64 << 24) as f64)
    }

    /// A value in `0..=max`, biased toward small values by raising a
    /// uniform fraction to `exponent` before scaling: the higher the
    /// exponent, the more the mass sits near zero. Keeps most generated
    /// trees and strings cheap while still drawing the occasional value
    /// near `max` to exercise the wire's ceilings.
    fn skewed(&mut self, max: usize, exponent: i32) -> usize {
        if max == 0 {
            return 0;
        }
        let biased = self.next_unit().powi(exponent);
        ((biased * max as f64) as usize).min(max)
    }

    fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next_range(items.len())]
    }
}

// ------------------------------------------------------------- tree generator

/// A hostile f32: sometimes a normal-looking value, sometimes one of the
/// exact values `sanitize` exists to handle (NaN, both infinities, the
/// float extremes, and out-of-range negatives).
fn gen_f32(rng: &mut Rng) -> f32 {
    match rng.next_range(20) {
        0 => f32::NAN,
        1 => f32::INFINITY,
        2 => f32::NEG_INFINITY,
        3 => f32::MAX,
        4 => f32::MIN,
        5 => -(rng.skewed(1_000_000, 2) as f32),
        _ => rng.skewed(20_000, 2) as f32 - 5_000.0,
    }
}

fn gen_opt_f32(rng: &mut Rng) -> Option<f32> {
    rng.next_bool().then(|| gen_f32(rng))
}

/// A key drawn from a small fixed pool: with only five options across a
/// whole tree, collisions are the common case rather than the exception.
fn gen_key(rng: &mut Rng) -> String {
    const POOL: [&str; 5] = ["App/a", "App/b", "dup", "x", "same-key"];
    (*rng.choose(&POOL)).to_string()
}

/// A string built from single-, two-, three- and four-byte UTF-8
/// characters. Most calls stay small so a tree of thousands of leaves
/// stays cheap to build; roughly one in three hundred goes hostile and
/// targets up to `3 * MAX_STRING_BYTES`, which is what actually exercises
/// `truncate`'s char-boundary walk.
fn gen_string(rng: &mut Rng) -> String {
    const POOL: [char; 6] = ['a', 'Z', 'é', '한', '😀', '\n'];
    let (cap, exponent) = match rng.next_range(300) {
        0 => (3 * MAX_STRING_BYTES, 8),
        _ => (48, 2),
    };
    let target = rng.skewed(cap, exponent);
    let mut s = String::new();
    while s.len() < target {
        s.push(*rng.choose(&POOL));
    }
    s
}

fn gen_opt_length(rng: &mut Rng) -> Option<Length> {
    rng.next_bool().then(|| match rng.next_range(4) {
        0 => Length::Fill,
        1 => Length::FillPortion(rng.next_range(1000) as u16),
        2 => Length::Shrink,
        _ => Length::Fixed(gen_f32(rng)),
    })
}

fn gen_opt_edges(rng: &mut Rng) -> Option<Edges> {
    rng.next_bool().then(|| Edges {
        top: gen_f32(rng),
        right: gen_f32(rng),
        bottom: gen_f32(rng),
        left: gen_f32(rng),
    })
}

fn gen_opt_color(rng: &mut Rng) -> Option<Rgba> {
    rng.next_bool()
        .then(|| Rgba([gen_f32(rng), gen_f32(rng), gen_f32(rng), gen_f32(rng)]))
}

fn gen_border(rng: &mut Rng) -> Border {
    Border {
        color: gen_opt_color(rng),
        width: gen_opt_f32(rng),
        radius: rng
            .next_bool()
            .then(|| [gen_f32(rng), gen_f32(rng), gen_f32(rng), gen_f32(rng)]),
    }
}

fn gen_opt_border(rng: &mut Rng) -> Option<Border> {
    rng.next_bool().then(|| gen_border(rng))
}

fn gen_opt_align_x(rng: &mut Rng) -> Option<AlignX> {
    rng.next_bool()
        .then(|| *rng.choose(&[AlignX::Left, AlignX::Center, AlignX::Right]))
}

fn gen_opt_align_y(rng: &mut Rng) -> Option<AlignY> {
    rng.next_bool()
        .then(|| *rng.choose(&[AlignY::Top, AlignY::Center, AlignY::Bottom]))
}

fn gen_axis(rng: &mut Rng) -> Axis {
    if rng.next_bool() {
        Axis::Column
    } else {
        Axis::Row
    }
}

fn gen_face(rng: &mut Rng) -> Face {
    Face {
        background: gen_opt_color(rng),
        text: gen_opt_color(rng),
        border: gen_opt_border(rng),
    }
}

fn gen_button_style(rng: &mut Rng) -> ButtonStyle {
    ButtonStyle {
        preset: [
            ButtonPreset::Primary,
            ButtonPreset::Secondary,
            ButtonPreset::Success,
            ButtonPreset::Warning,
            ButtonPreset::Danger,
            ButtonPreset::Text,
            ButtonPreset::Background,
            ButtonPreset::Subtle,
        ][rng.next_range(8)],
        recipe: rng.next_bool().then(|| ButtonRecipe {
            base: gen_face(rng),
            hover_background: gen_opt_color(rng),
            pressed_background: gen_opt_color(rng),
            disabled_background: gen_opt_color(rng),
            disabled_text: gen_opt_color(rng),
            disabled_opacity: gen_opt_f32(rng),
            focus_ring: gen_opt_color(rng),
            text_size: gen_opt_f32(rng),
            line_height: gen_opt_f32(rng),
            font: rng.next_bool().then(|| NamedFont {
                family: FontFamily::Named(gen_string(rng)),
                weight: Weight::Semibold,
                stretch: FontStretch::Normal,
                style: FontStyle::Normal,
            }),
        }),
        active: gen_face(rng),
        hovered: rng.next_bool().then(|| gen_face(rng)),
        pressed: rng.next_bool().then(|| gen_face(rng)),
        disabled: rng.next_bool().then(|| gen_face(rng)),
    }
}

fn gen_input_face(rng: &mut Rng) -> InputFace {
    InputFace {
        icon: gen_opt_color(rng),
        background: gen_opt_color(rng),
        border: gen_opt_border(rng),
        value: gen_opt_color(rng),
        placeholder: gen_opt_color(rng),
        selection: gen_opt_color(rng),
    }
}

fn gen_input_style(rng: &mut Rng) -> InputStyle {
    InputStyle {
        utility: gen_input_face(rng),
        focus_border: gen_opt_color(rng),
        focused_hovered: Some(gen_input_face(rng)),
        active: gen_input_face(rng),
        hovered: rng.next_bool().then(|| gen_input_face(rng)),
        focused: rng.next_bool().then(|| gen_input_face(rng)),
        disabled: rng.next_bool().then(|| gen_input_face(rng)),
    }
}

fn gen_opt_bool(rng: &mut Rng) -> Option<bool> {
    rng.next_bool().then(|| rng.next_bool())
}

fn gen_control_face(rng: &mut Rng) -> Option<ControlFace> {
    rng.next_bool().then(|| ControlFace {
        background: gen_opt_color(rng),
        mark: gen_opt_color(rng),
        text: gen_opt_color(rng),
        border: gen_opt_border(rng),
    })
}

fn gen_tone(rng: &mut Rng) -> Option<Tone> {
    rng.next_bool().then(|| {
        *rng.choose(&[
            Tone::Primary,
            Tone::Secondary,
            Tone::Success,
            Tone::Warning,
            Tone::Danger,
        ])
    })
}

fn gen_toggle_style(rng: &mut Rng) -> ToggleStyle {
    ToggleStyle {
        tone: gen_tone(rng),
        active_on: gen_control_face(rng),
        active_off: gen_control_face(rng),
        hovered_on: gen_control_face(rng),
        hovered_off: gen_control_face(rng),
        disabled_on: gen_control_face(rng),
        disabled_off: gen_control_face(rng),
    }
}

fn gen_radio_style(rng: &mut Rng) -> RadioStyle {
    RadioStyle {
        active_on: gen_control_face(rng),
        active_off: gen_control_face(rng),
        hovered_on: gen_control_face(rng),
        hovered_off: gen_control_face(rng),
    }
}

fn gen_opt_background(rng: &mut Rng) -> Option<Background> {
    rng.next_bool().then(|| {
        if rng.next_bool() {
            Background::Color(gen_opt_color(rng).unwrap_or(Rgba([0.0; 4])))
        } else {
            Background::Linear {
                angle: gen_f32(rng),
                stops: std::array::from_fn(|_| {
                    rng.next_bool().then(|| ColorStop {
                        offset: gen_f32(rng),
                        color: gen_opt_color(rng).unwrap_or(Rgba([0.0; 4])),
                    })
                }),
            }
        }
    })
}

fn gen_slider_face(rng: &mut Rng) -> Option<SliderFace> {
    rng.next_bool().then(|| SliderFace {
        rail_start: gen_opt_color(rng),
        rail_end: gen_opt_color(rng),
        rail_width: gen_opt_f32(rng),
        rail_border: gen_opt_border(rng),
        handle: gen_opt_color(rng),
        handle_border: gen_opt_border(rng),
        handle_shape: rng.next_bool().then(|| {
            if rng.next_bool() {
                SliderHandleShape::Circle {
                    radius: gen_f32(rng),
                }
            } else {
                SliderHandleShape::Rectangle {
                    width: rng.next_u64() as u16,
                    border_radius: [gen_f32(rng), gen_f32(rng), gen_f32(rng), gen_f32(rng)],
                }
            }
        }),
    })
}

fn gen_slider_style(rng: &mut Rng) -> SliderStyle {
    SliderStyle {
        active: gen_slider_face(rng),
        hovered: gen_slider_face(rng),
        dragged: gen_slider_face(rng),
    }
}

fn gen_pick_face(rng: &mut Rng) -> Option<PickFace> {
    rng.next_bool().then(|| PickFace {
        background: gen_opt_color(rng),
        text: gen_opt_color(rng),
        placeholder: gen_opt_color(rng),
        handle: gen_opt_color(rng),
        border: gen_opt_border(rng),
    })
}

fn gen_pick_list_style(rng: &mut Rng) -> PickListStyle {
    PickListStyle {
        active: gen_pick_face(rng),
        hovered: gen_pick_face(rng),
        opened: gen_pick_face(rng),
        opened_hovered: gen_pick_face(rng),
        menu: rng.next_bool().then(|| MenuFace {
            shadow: Shadow::default(),
            background: gen_opt_color(rng),
            text: gen_opt_color(rng),
            border: gen_opt_border(rng),
            selected_text: gen_opt_color(rng),
            selected_background: gen_opt_color(rng),
        }),
    }
}

fn gen_anchor(rng: &mut Rng) -> ScrollAnchor {
    *rng.choose(&[ScrollAnchor::Start, ScrollAnchor::End, ScrollAnchor::Keep])
}

fn gen_button_label(rng: &mut Rng) -> Node {
    Node::Button {
        checked: rng.next_bool().then(|| rng.next_bool()),
        expanded: rng.next_bool().then(|| rng.next_bool()),
        description: rng.next_bool().then(|| gen_string(rng)),
        key: gen_key(rng),
        content: ButtonContent::Label(gen_string(rng)),
        label: rng.next_bool().then(|| gen_string(rng)),
        on_press: rng.next_bool().then(|| rng.next_u64() as u32),
        width: gen_opt_length(rng),
        height: gen_opt_length(rng),
        padding: gen_opt_edges(rng),
        style: gen_button_style(rng),
    }
}

fn gen_input(rng: &mut Rng) -> Node {
    Node::Input {
        options: InputOptions {
            label: gen_string(rng),
            description: Some(gen_string(rng)),
            disabled: rng.next_bool(),
            padding: gen_opt_edges(rng),
            text_size: gen_opt_f32(rng),
            line_height: gen_opt_f32(rng),
            align: Some(AlignX::Center),
            font: Some(NamedFont {
                family: FontFamily::Named(gen_string(rng)),
                weight: Weight::Normal,
                stretch: FontStretch::Normal,
                style: FontStyle::Normal,
            }),
        },
        key: gen_key(rng),
        placeholder: gen_string(rng),
        value: gen_string(rng),
        on_input: rng.next_u64() as u32,
        on_submit: rng.next_bool().then(|| rng.next_u64() as u32),
        width: gen_opt_length(rng),
        secure: rng.next_bool(),
        style: Box::new(gen_input_style(rng)),
    }
}

/// One logical document per identifier, so every reference the tree makes to
/// the same document is exactly the one `validate_editor_document_refs`
/// requires. Byte lengths stay small: a projection is charged per binding, and
/// the fuzz is about tree shape, not the aggregate byte ceilings the focused
/// `editor_document` tests already pin.
fn gen_document(rng: &mut Rng) -> editor_document::EditorDocumentRef {
    const POOL: [&str; 5] = ["app:draft", "app:notes", "dup", "x", "app:same"];
    let index = rng.next_range(POOL.len());
    let byte_len = (index * 37) as u32;
    editor_document::EditorDocumentRef {
        document: POOL[index].to_string(),
        reset: index as u64,
        text_revision: 2 * index as u64,
        revision: 3 * index as u64,
        cursor: EditorCursor {
            position: EditorPosition {
                line: 0,
                column: byte_len,
            },
            selection: None,
        },
        byte_len,
    }
}

fn gen_editor(rng: &mut Rng) -> Node {
    Node::Editor {
        document: gen_document(rng),
        on_document: rng.next_u64() as u32,
        editable: rng.next_bool(),
        options: Box::new(EditorOptions {
            rich: None,
            presentation: None,
            binding: None,
            size: gen_opt_f32(rng),
            padding: gen_opt_f32(rng),
            line_height: Some(if rng.next_bool() {
                LineHeight::Relative(gen_f32(rng))
            } else {
                LineHeight::Absolute(gen_f32(rng))
            }),
            wrapping: Some(Wrapping::Word),
            font: Some(NamedFont {
                family: FontFamily::Named(gen_string(rng)),
                weight: Weight::Normal,
                stretch: FontStretch::Normal,
                style: FontStyle::Normal,
            }),
            style: gen_input_style(rng),
        }),
        key: gen_key(rng),
        placeholder: gen_string(rng),
        width: rng.next_bool().then(|| gen_f32(rng)),
        height: gen_opt_length(rng),
        min_height: rng.next_bool().then(|| gen_f32(rng)),
        max_height: rng.next_bool().then(|| gen_f32(rng)),
    }
}

fn gen_rule(rng: &mut Rng) -> Node {
    Node::Rule {
        key: gen_key(rng),
        axis: gen_axis(rng),
        thickness: gen_f32(rng),
        color: gen_opt_color(rng),
        weak: rng.next_bool(),
        radius: rng
            .next_bool()
            .then(|| [gen_f32(rng), gen_f32(rng), gen_f32(rng), gen_f32(rng)]),
        snap: gen_opt_bool(rng),
    }
}

fn gen_text(rng: &mut Rng) -> Node {
    Node::Text {
        options: Default::default(),
        key: gen_key(rng),
        content: gen_string(rng),
        size: gen_opt_f32(rng),
        color: gen_opt_color(rng),
        font: Font {
            monospace: rng.next_bool(),
            weight: *rng.choose(&[
                Weight::Normal,
                Weight::Medium,
                Weight::Semibold,
                Weight::Bold,
            ]),
        },
        width: gen_opt_length(rng),
        align_x: gen_opt_align_x(rng),
    }
}

/// A picture whose bytes cross about half the time, and about one time in
/// sixteen run past `MAX_PICTURE_BYTES_PER_FRAME` on their own.
fn gen_svg(rng: &mut Rng) -> Node {
    let bytes = rng.next_bool().then(|| {
        let len = match rng.next_range(16) {
            0 => MAX_PICTURE_BYTES_PER_FRAME + 1 + rng.next_range(64),
            _ => rng.skewed(4096, 2),
        };
        vec![b'<'; len]
    });
    if rng.next_bool() {
        let data = bytes.map(|bytes| {
            if rng.next_bool() {
                ImageData::Encoded(bytes)
            } else {
                ImageData::Rgba {
                    width: rng.next_u64() as u32,
                    height: rng.next_u64() as u32,
                    pixels: bytes,
                }
            }
        });
        if rng.next_bool() {
            return Node::ImageViewer {
                key: gen_key(rng),
                hash: rng.next_u64(),
                data,
                label: rng.next_bool().then(|| gen_string(rng)),
                fit: None,
                filter: ImageFilter::Nearest,
                width: gen_opt_length(rng),
                height: gen_opt_length(rng),
                options: ViewerOptions {
                    padding: gen_opt_f32(rng),
                    scale_bounds: rng.next_bool().then(|| (gen_f32(rng), gen_f32(rng))),
                    scale_step: gen_opt_f32(rng),
                },
            };
        }
        return Node::Image {
            key: gen_key(rng),
            hash: rng.next_u64(),
            data,
            label: rng.next_bool().then(|| gen_string(rng)),
            fit: None,
            rotation: Some(Rotation::Solid(gen_f32(rng))),
            opacity: Some(gen_f32(rng)),
            filter: ImageFilter::Linear,
            width: gen_opt_length(rng),
            height: gen_opt_length(rng),
        };
    }
    Node::Svg {
        inherit_button_ink: true,
        key: gen_key(rng),
        hash: rng.next_u64(),
        bytes,
        label: rng.next_bool().then(|| gen_string(rng)),
        color: gen_opt_color(rng),
        hover: rng.next_bool().then(|| gen_opt_color(rng)),
        fit: rng.next_bool().then(|| {
            *rng.choose(&[
                ContentFit::Contain,
                ContentFit::Cover,
                ContentFit::Fill,
                ContentFit::None,
                ContentFit::ScaleDown,
            ])
        }),
        rotation: rng.next_bool().then(|| match rng.next_bool() {
            true => Rotation::Floating(gen_f32(rng)),
            false => Rotation::Solid(gen_f32(rng)),
        }),
        opacity: gen_opt_f32(rng),
        width: gen_opt_length(rng),
        height: gen_opt_length(rng),
    }
}

fn gen_toggle(rng: &mut Rng) -> Node {
    Node::Toggle {
        key: gen_key(rng),
        kind: *rng.choose(&[ToggleKind::Checkbox, ToggleKind::Switch]),
        label: gen_string(rng),
        checked: rng.next_bool(),
        on_toggle: rng.next_bool().then(|| rng.next_u64() as u32),
        width: gen_opt_length(rng),
        style: gen_toggle_style(rng),
    }
}

fn gen_radio(rng: &mut Rng) -> Node {
    Node::Radio {
        key: gen_key(rng),
        label: gen_string(rng),
        selected: rng.next_bool(),
        on_select: rng.next_u64() as u32,
        width: gen_opt_length(rng),
        style: gen_radio_style(rng),
    }
}

fn gen_slider(rng: &mut Rng) -> Node {
    Node::Slider {
        key: gen_key(rng),
        value: gen_f32(rng),
        min: gen_f32(rng),
        max: gen_f32(rng),
        step: gen_f32(rng),
        on_change: rng.next_u64() as u32,
        on_release: rng.next_bool().then(|| rng.next_u64() as u32),
        axis: gen_axis(rng),
        width: gen_opt_length(rng),
        height: gen_opt_length(rng),
        style: gen_slider_style(rng),
    }
}

/// A pick list whose option count crosses `MAX_OPTIONS` about one time in
/// eight, and whose selection points anywhere, including past the list.
fn gen_pick_list(rng: &mut Rng) -> Node {
    let count = match rng.next_range(8) {
        0 => MAX_OPTIONS + 1 + rng.next_range(64),
        _ => rng.skewed(16, 2),
    };
    Node::PickList {
        settings: Default::default(),
        key: gen_key(rng),
        options: (0..count).map(|_| gen_string(rng)).collect(),
        selected: rng
            .next_bool()
            .then(|| rng.next_range(2 * MAX_OPTIONS) as u32),
        placeholder: rng.next_bool().then(|| gen_string(rng)),
        on_select: rng.next_u64() as u32,
        width: gen_opt_length(rng),
        style: gen_pick_list_style(rng),
    }
}

fn gen_progress(rng: &mut Rng) -> Node {
    Node::Progress {
        key: gen_key(rng),
        value: gen_f32(rng),
        min: gen_f32(rng),
        max: gen_f32(rng),
        axis: gen_axis(rng),
        length: gen_opt_length(rng),
        girth: gen_opt_length(rng),
        tone: gen_tone(rng),
        background: gen_opt_color(rng),
        bar: gen_opt_color(rng),
        border: gen_opt_border(rng),
    }
}

/// A leaf with no children, for filling out a wide `Linear`: every leaf
/// variant except `Space` carries a string, a colour or a number worth
/// pulling into range.
fn gen_surface(rng: &mut Rng) -> Node {
    Node::Surface {
        key: gen_key(rng),
        name: gen_string(rng),
        args: vec![
            view_wire::SurfaceValue::Str(gen_string(rng)),
            view_wire::SurfaceValue::F64(f64::NAN),
        ],
        on_event: Some(0),
    }
}

fn gen_leaf(rng: &mut Rng) -> Node {
    match rng.next_range(12) {
        0 => gen_text(rng),
        10 => gen_svg(rng),
        11 => gen_editor(rng),
        1 => Node::Space {
            width: gen_opt_length(rng),
            height: gen_opt_length(rng),
        },
        2 => gen_input(rng),
        3 => gen_rule(rng),
        4 => gen_toggle(rng),
        5 => gen_radio(rng),
        6 => gen_slider(rng),
        7 => gen_pick_list(rng),
        8 => gen_progress(rng),
        9 => gen_surface(rng),
        _ => gen_button_label(rng),
    }
}

/// Builds one random tree of exactly `depth` levels of nesting with `width`
/// extra siblings injected at one random level, entirely with an
/// iterative loop rather than recursion — the wire's own stress test
/// (`deep_chain_bytes` in `lib.rs`) builds a deep chain the same way,
/// because a recursive builder would blow its own stack before `decode`
/// ever got a chance to refuse anything.
/// Either of the two nodes holding a child list, around `children`.
fn gen_list(rng: &mut Rng, children: Vec<Node>) -> Node {
    if rng.next_range(3) == 0 {
        let count = children.len();
        return Node::KeyedColumn {
            background: gen_opt_color(rng),
            border: gen_opt_border(rng),
            key: gen_key(rng),
            keys: Some(
                (0..count)
                    .map(|i| view_wire::ListKey::Integer(i as i64))
                    .collect(),
            ),
            spacing: gen_opt_f32(rng),
            padding: gen_opt_edges(rng),
            width: gen_opt_length(rng),
            height: gen_opt_length(rng),
            max_width: gen_opt_f32(rng),
            align: gen_opt_align_x(rng),
            virtual_row: gen_opt_f32(rng),
            children,
        };
    }
    match rng.next_range(5) {
        0 => {
            return Node::Hover {
                key: gen_key(rng),
                width: gen_opt_length(rng),
                height: gen_opt_length(rng),
                padding: gen_opt_edges(rng),
                background: gen_opt_color(rng),
                border: gen_opt_border(rng),
                tint: gen_opt_color(rng),
                radius: gen_f32(rng),
                open: rng.next_bool(),
                children,
            };
        }
        1 => {
            return Node::Overlay {
                key: gen_key(rng),
                padding: gen_f32(rng),
                backdrop: Rgba([gen_f32(rng), gen_f32(rng), gen_f32(rng), gen_f32(rng)]),
                align_x: gen_opt_align_x(rng).unwrap_or(AlignX::Center),
                align_y: gen_opt_align_y(rng).unwrap_or(AlignY::Center),
                on_dismiss: Some(rng.next_u64() as u32),
                children,
            };
        }
        _ => {}
    }
    if rng.next_range(3) == 0 {
        return Node::Stack {
            key: gen_key(rng),
            width: gen_opt_length(rng),
            height: gen_opt_length(rng),
            padding: gen_opt_edges(rng),
            background: gen_opt_color(rng),
            border: gen_opt_border(rng),
            clip: rng.next_bool(),
            under: rng.next_u64() as u32,
            children,
        };
    }
    if rng.next_bool() {
        return Node::Linear {
            max_width: gen_opt_f32(rng),
            clip: rng.next_bool(),
            wrap: rng.next_bool().then(|| Wrap {
                spacing: gen_opt_f32(rng),
                align: gen_opt_align_x(rng),
            }),
            key: gen_key(rng),
            axis: gen_axis(rng),
            spacing: gen_opt_f32(rng),
            padding: gen_opt_edges(rng),
            width: gen_opt_length(rng),
            height: gen_opt_length(rng),
            align: gen_opt_align_x(rng),
            background: gen_opt_color(rng),
            border: gen_opt_border(rng),
            children,
        };
    }
    Node::Grid {
        key: gen_key(rng),
        columns: rng.next_bool().then(|| rng.next_u64() as u32),
        fluid: gen_opt_f32(rng),
        spacing: gen_opt_f32(rng),
        padding: gen_opt_edges(rng),
        width: gen_opt_length(rng),
        height: gen_opt_length(rng),
        aspect: gen_opt_f32(rng),
        background: gen_opt_color(rng),
        border: gen_opt_border(rng),
        children,
    }
}

fn gen_tree(rng: &mut Rng, depth: usize, width: usize) -> Node {
    let mut node = gen_leaf(rng);
    let width_level = if depth == 0 { 0 } else { rng.next_range(depth) };
    for level in 0..depth {
        if level == width_level && width > 0 {
            let mut children: Vec<Node> = (0..width).map(|_| gen_leaf(rng)).collect();
            children.push(node);
            node = gen_list(rng, children);
            continue;
        }
        node = match rng.next_range(8) {
            7 => Node::ResizeHandle {
                key: gen_key(rng),
                on_press: rng.next_bool().then(|| rng.next_u64() as u32),
                on_release: rng.next_bool().then(|| rng.next_u64() as u32),
                on_drag: rng.next_bool().then(|| rng.next_u64() as u32),
                cursor: Some(mouse::Cursor::ResizingHorizontally),
                content: Box::new(node),
            },
            6 => Node::Tooltip {
                key: gen_key(rng),
                position: TooltipPosition::Bottom,
                gap: gen_f32(rng),
                padding: gen_f32(rng),
                delay_ms: rng.next_u64(),
                snap: rng.next_bool(),
                style: TooltipStyle {
                    preset: TooltipPreset::Transparent,
                    background: gen_opt_color(rng),
                    text: gen_opt_color(rng),
                    border: gen_opt_border(rng),
                    shadow: Shadow {
                        color: gen_opt_color(rng),
                        x: gen_opt_f32(rng),
                        y: gen_opt_f32(rng),
                        blur: gen_opt_f32(rng),
                    },
                    pixel_snap: gen_opt_bool(rng),
                },
                children: vec![node, gen_leaf(rng)],
            },
            4 => Node::Sensor {
                key: gen_key(rng),
                reset: None,
                on_show: rng.next_bool().then(|| rng.next_u64() as u32),
                on_resize: rng.next_bool().then(|| rng.next_u64() as u32),
                on_hide: rng.next_bool().then(|| rng.next_u64() as u32),
                anticipate: gen_opt_f32(rng),
                delay: gen_opt_f32(rng),
                child: Box::new(node),
            },
            5 => Node::MouseArea {
                key: gen_key(rng),
                on_press: rng.next_bool().then(|| rng.next_u64() as u32),
                on_release: rng.next_bool().then(|| rng.next_u64() as u32),
                on_double_click: None,
                on_right_press: None,
                on_right_release: None,
                on_middle_press: None,
                on_middle_release: None,
                on_enter: rng.next_bool().then(|| rng.next_u64() as u32),
                on_exit: None,
                on_move: rng.next_bool().then(|| rng.next_u64() as u32),
                on_press_at: None,
                on_scroll: rng.next_bool().then(|| rng.next_u64() as u32),
                content: Box::new(node),
            },
            0 => Node::Container {
                shadow: Shadow {
                    color: gen_opt_color(rng),
                    x: gen_opt_f32(rng),
                    y: gen_opt_f32(rng),
                    blur: gen_opt_f32(rng),
                },
                max_width: None,
                max_height: None,
                clip: false,
                key: gen_key(rng),
                width: gen_opt_length(rng),
                height: gen_opt_length(rng),
                padding: gen_opt_edges(rng),
                align_x: gen_opt_align_x(rng),
                align_y: gen_opt_align_y(rng),
                background: gen_opt_background(rng),
                border: gen_opt_border(rng),
                snap: gen_opt_bool(rng),
                content: Box::new(node),
            },
            1 => gen_list(rng, vec![node]),
            2 => Node::Scroll {
                on_scroll: Some(7),
                virtual_rows: rng.next_bool(),
                key: gen_key(rng),
                direction: *rng.choose(&[
                    ScrollDirection::Vertical,
                    ScrollDirection::Horizontal,
                    ScrollDirection::Both,
                ]),
                width: gen_opt_length(rng),
                height: gen_opt_length(rng),
                bar_hidden: rng.next_bool(),
                bar_width: gen_opt_f32(rng),
                bar_margin: gen_opt_f32(rng),
                scroller_width: gen_opt_f32(rng),
                bar_spacing: gen_opt_f32(rng),
                anchor_x: gen_anchor(rng),
                anchor_y: gen_anchor(rng),
                auto_scroll: rng.next_bool(),
                background: gen_opt_color(rng),
                border: gen_opt_border(rng),
                content: Box::new(node),
            },
            _ => Node::Button {
                checked: rng.next_bool().then(|| rng.next_bool()),
                expanded: rng.next_bool().then(|| rng.next_bool()),
                description: rng.next_bool().then(|| gen_string(rng)),
                key: gen_key(rng),
                content: ButtonContent::Child(Box::new(node)),
                label: rng.next_bool().then(|| gen_string(rng)),
                on_press: rng.next_bool().then(|| rng.next_u64() as u32),
                width: gen_opt_length(rng),
                height: gen_opt_length(rng),
                padding: gen_opt_edges(rng),
                style: gen_button_style(rng),
            },
        };
    }
    node
}

/// One random `Frame` around a tree of exactly `depth`/`width`: the shared
/// core behind both [`gen_frame`] (which chooses depth/width to stress the
/// decode-time and sanitize-time ceilings) and [`gen_frame_bounded`] (which
/// keeps trees small because its callers re-encode and mutate them
/// hundreds of times each).
fn gen_frame_with(rng: &mut Rng, depth: usize, width: usize) -> Frame {
    let root = gen_tree(rng, depth, width);
    let requests = (0..rng.next_range(4))
        .map(|_| Request {
            id: rng.next_u64(),
            kind: gen_string(rng),
            payload: (0..rng.next_range(16))
                .map(|_| rng.next_range(256) as u8)
                .collect(),
        })
        .collect();
    let cancels = (0..rng.next_range(4)).map(|_| rng.next_u64()).collect();
    Frame {
        upstream_sanitization: Default::default(),
        editor_decisions: Vec::new(),
        editor_documents: Vec::new(),
        mouse_interest: rng.next_bool(),
        event_interest: Default::default(),
        root: Some(root),
        requests,
        cancels,
        unchanged: rng.next_bool(),
        busy: rng.next_bool(),
        patches: Vec::new(),
    }
}

// ------------------------------------------------------------ patch generator

/// A path into `root`: a real one (a random walk down the tree that stops
/// at a random depth) or, from a `hostile` sender, sometimes one step past
/// a real one or a random vector, so `apply` sees both the paths a guest
/// sends and the ones a hostile one does.
fn gen_path(rng: &mut Rng, root: &Node, hostile: bool) -> Vec<u32> {
    let mut path = Vec::new();
    let mut node = root;
    while !node.children().is_empty() && rng.next_range(4) != 0 {
        let index = rng.next_range(node.children().len());
        path.push(index as u32);
        node = &node.children()[index];
    }
    match rng.next_range(12) {
        0 if hostile => path.push(rng.next_range(4) as u32),
        1 if hostile => {
            path = (0..rng.next_range(4))
                .map(|_| rng.next_range(8) as u32)
                .collect()
        }
        _ => {}
    }
    path
}

/// A subtree for a patch to carry: usually small and cheap, occasionally
/// as hostile as [`gen_tree`] goes, so an inserted subtree can push the
/// tree past every ceiling on its own.
fn gen_patch_tree(rng: &mut Rng) -> Node {
    let (depth, width) = match rng.next_range(40) {
        0 => (MAX_DEPTH + 4, MAX_NODES / 4),
        _ => (rng.skewed(4, 2), rng.skewed(6, 2)),
    };
    gen_tree(rng, depth, width)
}

/// One random patch against `root` as it stands. A `hostile` sender's
/// indices are sometimes past the list, its list edits sometimes aimed at
/// a node with no list, and its `Props` sometimes a whole subtree; the
/// other kind of sender is what a real diff emits, so a whole sequence of
/// its patches applies and the invariant is checked on the result.
fn gen_patch(rng: &mut Rng, root: &Node, hostile: bool) -> Patch {
    let path = gen_path(rng, root, hostile);
    let mut node = Some(root);
    for index in &path {
        node = node.and_then(|node| node.children().get(*index as usize));
    }
    let is_list = node.is_some_and(|node| {
        matches!(
            node,
            Node::Linear { .. }
                | Node::Grid { .. }
                | Node::KeyedColumn { .. }
                | Node::Flex { .. }
                | Node::Stack { .. }
                | Node::Hover { .. }
                | Node::Overlay { .. }
        )
    });
    let len = node.map_or(0, |node| node.children().len());
    let index = |rng: &mut Rng, bound: usize| match rng.next_range(8) {
        0 if hostile => rng.next_range(bound + 3) as u32,
        _ => rng.next_range(bound.max(1)) as u32,
    };
    let kind = match (is_list || hostile, rng.next_range(5)) {
        (false, kind) => kind % 2,
        // Nothing to remove or move in an empty list.
        (true, kind) if kind >= 3 && len == 0 && !hostile => 2,
        (true, kind) => kind,
    };
    match kind {
        0 => Patch::Replace {
            path,
            node: gen_patch_tree(rng),
        },
        1 => {
            // A node with its children set aside, as a guest sends it, of
            // the arity the node at the path has — or, from a hostile
            // sender, any node at all.
            let mut fresh = gen_patch_tree(rng);
            let same_arity = |fresh: &Node| match (node, fresh) {
                (
                    Some(
                        Node::Linear { .. }
                        | Node::Grid { .. }
                        | Node::KeyedColumn { .. }
                        | Node::Flex { .. }
                        | Node::Stack { .. }
                        | Node::Hover { .. }
                        | Node::Overlay { .. },
                    ),
                    Node::Linear { .. }
                    | Node::Grid { .. }
                    | Node::KeyedColumn { .. }
                    | Node::Flex { .. }
                    | Node::Stack { .. }
                    | Node::Hover { .. }
                    | Node::Overlay { .. },
                ) => true,
                (Some(at), fresh) => {
                    !matches!(
                        at,
                        Node::Linear { .. }
                            | Node::Grid { .. }
                            | Node::KeyedColumn { .. }
                            | Node::Flex { .. }
                            | Node::Stack { .. }
                            | Node::Hover { .. }
                            | Node::Overlay { .. }
                    ) && !matches!(
                        fresh,
                        Node::Linear { .. }
                            | Node::Grid { .. }
                            | Node::KeyedColumn { .. }
                            | Node::Flex { .. }
                            | Node::Stack { .. }
                            | Node::Hover { .. }
                            | Node::Overlay { .. }
                    ) && at.children().len() == fresh.children().len()
                }
                (None, _) => false,
            };
            if !hostile {
                while !same_arity(&fresh) {
                    fresh = gen_patch_tree(rng);
                }
            }
            if hostile && rng.next_range(4) == 0 {
                return Patch::Props { path, node: fresh };
            }
            for child in fresh.children_mut() {
                *child = Node::empty();
            }
            if let Node::Linear { children, .. } = &mut fresh {
                children.clear();
            }
            Patch::Props { path, node: fresh }
        }
        2 => Patch::Insert {
            path,
            index: index(rng, len + 1),
            node: gen_patch_tree(rng),
        },
        3 => Patch::Remove {
            path,
            index: index(rng, len),
        },
        _ => Patch::Move {
            path,
            from: index(rng, len),
            to: index(rng, len),
        },
    }
}

/// One random `Frame`: a tree plus a handful of requests whose `kind`
/// string is generated the same hostile way as everything else.
fn gen_frame(rng: &mut Rng, i: usize) -> Frame {
    let (depth, width) = if i == 0 {
        // Exactly one tree per run goes just over each door, not far over
        // it, and only once: `sanitize`'s key-collision renaming (`claim`
        // in lib.rs) costs quadratic time in how many nodes share one base
        // key once uniquing runs out of room, this file's key pool is
        // deliberately tiny (collisions are the point), and `sanitize`
        // stops at MAX_NODES regardless of how much wider the input tree
        // claims to be — so paying that quadratic cost even once per
        // saturating tree is unavoidable, and paying it many times over
        // (the previous `3 * MAX_NODES`, every 25th tree) is just wasted
        // wall clock, not more coverage.
        (MAX_DEPTH + 8, MAX_NODES + 300)
    } else {
        // A cap and an exponent chosen so this branch, which runs for
        // every other tree, essentially never saturates `sanitize`'s
        // MAX_NODES budget on its own — the forced tree above is what
        // guarantees a saturating, over-both-ceilings tree is exercised.
        (rng.skewed(2 * MAX_DEPTH, 6), rng.skewed(MAX_NODES / 4, 6))
    };
    gen_frame_with(rng, depth, width)
}

/// A frame capped well below [`gen_frame`]'s ceiling-stressing sizes: only
/// [`mutated_bytes_never_panic`] calls this, and it re-encodes and mutates
/// each frame hundreds of times, so a frame in the hundreds-of-KB range
/// (which `gen_frame`'s skew occasionally draws) turns a few hundred
/// `decode` calls into seconds each. The bomb itself — a claimed size with
/// no data behind it — is what exercises decode's refusal path; a tree
/// actually built this wide adds nothing that `gen_frame`'s own forced
/// giants (covered by `random_trees_come_out_of_sanitize_inside_every_bound`)
/// don't already cover.
fn gen_frame_bounded(rng: &mut Rng) -> Frame {
    let depth = rng.skewed(MAX_DEPTH / 2, 3);
    let width = rng.skewed(MAX_NODES / 64, 6);
    gen_frame_with(rng, depth, width)
}

/// Runs `f` on a thread with a much larger stack than a test gets by
/// default, then re-raises whatever it did (return value or panic) on the
/// caller — a panic keeps its original message, seed included, instead of
/// being replaced by a generic "thread panicked" one. Building and encoding
/// a tree recurses once per level of nesting the same way decoding does
/// (see `lib.rs`'s own `deep_chain_bytes`), so the frames this file builds
/// up to `2 * MAX_DEPTH` levels deep get the same headroom.
fn on_big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let handle = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(f)
        .expect("spawn a big-stack thread");
    match handle.join() {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn build_frame(seed: u64, i: usize) -> Frame {
    on_big_stack(move || {
        let mut rng = Rng::new(seed);
        gen_frame(&mut rng, i)
    })
}

fn build_and_encode(seed: u64, i: usize) -> (Frame, Vec<u8>) {
    on_big_stack(move || {
        let mut rng = Rng::new(seed);
        let frame = gen_frame(&mut rng, i);
        let bytes = encode(&frame);
        (frame, bytes)
    })
}

fn build_and_encode_bounded(seed: u64) -> (Frame, Vec<u8>) {
    on_big_stack(move || {
        let mut rng = Rng::new(seed);
        let frame = gen_frame_bounded(&mut rng);
        let bytes = encode(&frame);
        (frame, bytes)
    })
}

// -------------------------------------------------------- bound assertions

/// The nesting depth `sanitize` would count for this node (root is 0, each
/// `Container`/`Scroll`/`Linear`/`Grid`/`Button` child adds one) — the same metric
/// `decode`'s own depth budget counts, so it doubles as "was this tree
/// really over the door" evidence when `decode` refuses one.
fn tree_depth(node: &Node) -> usize {
    match node {
        Node::Container { content, .. }
        | Node::Sensor { child: content, .. }
        | Node::MouseArea { content, .. }
        | Node::ResizeHandle { content, .. }
        | Node::Pin { content, .. }
        | Node::Float { content, .. }
        | Node::Responsive { content, .. }
        | Node::Lazy { content, .. }
        | Node::Scroll { content, .. } => 1 + tree_depth(content),
        Node::Linear { children, .. }
        | Node::Grid { children, .. }
        | Node::KeyedColumn { children, .. }
        | Node::Flex { children, .. }
        | Node::Stack { children, .. }
        | Node::Hover { children, .. }
        | Node::Tooltip { children, .. }
        | Node::Overlay { children, .. }
        | Node::When { children, .. } => 1 + children.iter().map(tree_depth).max().unwrap_or(0),
        Node::Button {
            content: ButtonContent::Child(child),
            ..
        } => 1 + tree_depth(child),
        _ => 0,
    }
}

fn check_length(length: &Option<Length>, ctx: &str) {
    if let Some(Length::Fixed(value)) = length {
        assert!(
            value.is_finite() && (0.0..=PIXEL_BOUND).contains(value),
            "{ctx}: length {value} outside 0..={PIXEL_BOUND}"
        );
    }
}

fn check_edges(edges: &Option<Edges>, ctx: &str) {
    let Some(edges) = edges else { return };
    for value in [edges.top, edges.right, edges.bottom, edges.left] {
        assert!(
            value.is_finite() && (0.0..=PIXEL_BOUND).contains(&value),
            "{ctx}: edge {value} outside 0..={PIXEL_BOUND}"
        );
    }
}

fn check_color(color: &Option<Rgba>, ctx: &str) {
    let Some(Rgba(channels)) = color else { return };
    for value in channels {
        assert!(
            value.is_finite() && (0.0..=1.0).contains(value),
            "{ctx}: colour channel {value} outside 0..=1"
        );
    }
}

fn check_border(border: &Option<Border>, ctx: &str) {
    let Some(border) = border else { return };
    check_color(&border.color, ctx);
    if let Some(width) = border.width {
        assert!(
            width.is_finite() && (0.0..=PIXEL_BOUND).contains(&width),
            "{ctx}: border width {width} outside 0..={PIXEL_BOUND}"
        );
    }
    for radius in border.radius.into_iter().flatten() {
        assert!(
            radius.is_finite() && (0.0..=PIXEL_BOUND).contains(&radius),
            "{ctx}: border radius {radius} outside 0..={PIXEL_BOUND}"
        );
    }
}

/// A slider or progress number: finite, and nothing more is promised.
fn check_finite(value: f32, ctx: &str, field: &str) {
    assert!(value.is_finite(), "{ctx}: {field} {value} is not finite");
}

fn check_pixels(value: &Option<f32>, ctx: &str, field: &str) {
    if let Some(value) = value {
        assert!(
            value.is_finite() && (0.0..=PIXEL_BOUND).contains(value),
            "{ctx}: {field} {value} outside 0..={PIXEL_BOUND}"
        );
    }
}

fn check_control_face(face: &Option<ControlFace>, ctx: &str) {
    let Some(face) = face else { return };
    check_color(&face.background, ctx);
    check_color(&face.mark, ctx);
    check_color(&face.text, ctx);
    check_border(&face.border, ctx);
}

fn check_background(value: &Option<Background>, ctx: &str) {
    match value {
        Some(Background::Color(color)) => check_color(&Some(*color), ctx),
        Some(Background::Linear { angle, stops }) => {
            assert!(angle.is_finite(), "{ctx}: gradient angle must be finite");
            let mut previous = None;
            for stop in stops.iter().flatten() {
                assert!(
                    stop.offset.is_finite() && (0.0..=1.0).contains(&stop.offset),
                    "{ctx}: gradient stop must be finite and within 0..=1"
                );
                assert!(
                    previous.is_none_or(|offset| stop.offset > offset),
                    "{ctx}: gradient stops must increase"
                );
                previous = Some(stop.offset);
                check_color(&Some(stop.color), ctx);
            }
        }
        None => {}
    }
}

fn check_slider_face(face: &Option<SliderFace>, ctx: &str) {
    let Some(face) = face else { return };
    check_color(&face.rail_start, ctx);
    check_color(&face.rail_end, ctx);
    check_pixels(&face.rail_width, ctx, "rail width");
    check_border(&face.rail_border, ctx);
    check_color(&face.handle, ctx);
    check_border(&face.handle_border, ctx);
    if let Some(shape) = &face.handle_shape {
        let radii: &[f32] = match shape {
            SliderHandleShape::Circle { radius } => std::slice::from_ref(radius),
            SliderHandleShape::Rectangle { border_radius, .. } => border_radius,
        };
        for radius in radii {
            check_pixels(&Some(*radius), ctx, "slider handle radius");
        }
    }
}

fn check_pick_face(face: &Option<PickFace>, ctx: &str) {
    let Some(face) = face else { return };
    check_color(&face.background, ctx);
    check_color(&face.text, ctx);
    check_color(&face.placeholder, ctx);
    check_color(&face.handle, ctx);
    check_border(&face.border, ctx);
}

fn check_string(text: &str, ctx: &str, field: &str) {
    assert!(
        text.len() <= MAX_STRING_BYTES,
        "{ctx}: {field} is {} bytes, over MAX_STRING_BYTES",
        text.len()
    );
    assert!(
        text.is_char_boundary(text.len()),
        "{ctx}: {field} does not end on a char boundary"
    );
}

/// Walks a sanitized tree asserting every post-condition `sanitize_node`
/// promises: depth within `MAX_DEPTH`, every string within
/// `MAX_STRING_BYTES` and on a char boundary, every key unique across the
/// whole tree, every size/colour/border field inside its own bound, and the
/// picture bytes the tree carries summed into `svg_bytes`.
fn check_bounds(
    node: &Node,
    depth: usize,
    keys: &mut HashSet<String>,
    svg_bytes: &mut usize,
    ctx: &str,
) {
    assert!(
        depth <= MAX_DEPTH,
        "{ctx}: a node sits at depth {depth}, over MAX_DEPTH"
    );
    if let Some(key) = node.key() {
        check_string(key, ctx, "key");
        assert!(
            keys.insert(key.to_string()),
            "{ctx}: key {key:?} used more than once after sanitize"
        );
    }
    match node {
        Node::Container {
            shadow,
            width,
            height,
            padding,
            background,
            border,
            content,
            snap: _,
            ..
        } => {
            check_color(&shadow.color, ctx);
            check_pixels(&shadow.blur, ctx, "box shadow blur");
            for value in [shadow.x, shadow.y].into_iter().flatten() {
                assert!(value.is_finite() && (-PIXEL_BOUND..=PIXEL_BOUND).contains(&value));
            }
            check_length(width, ctx);
            check_length(height, ctx);
            check_edges(padding, ctx);
            check_background(background, ctx);
            check_border(border, ctx);
            check_bounds(content, depth + 1, keys, svg_bytes, ctx);
        }
        Node::Float {
            scale,
            shadow,
            radius,
            content,
            ..
        } => {
            assert!(scale.is_finite() && (f32::EPSILON..=PIXEL_BOUND).contains(scale));
            check_color(&shadow.color, ctx);
            check_pixels(&shadow.blur, ctx, "float shadow blur");
            for value in [shadow.x, shadow.y].into_iter().flatten() {
                assert!(value.is_finite() && (-PIXEL_BOUND..=PIXEL_BOUND).contains(&value));
            }
            for value in radius.iter().flatten() {
                check_pixels(&Some(*value), ctx, "float shadow radius");
            }
            check_bounds(content, depth + 1, keys, svg_bytes, ctx);
        }
        Node::Pin {
            x,
            y,
            width,
            height,
            content,
            ..
        } => {
            assert!(x.is_finite() && x.abs() <= PIXEL_BOUND);
            assert!(y.is_finite() && y.abs() <= PIXEL_BOUND);
            check_length(width, ctx);
            check_length(height, ctx);
            check_bounds(content, depth + 1, keys, svg_bytes, ctx);
        }
        Node::Tooltip {
            gap,
            padding,
            delay_ms,
            style,
            children,
            ..
        } => {
            check_pixels(&Some(*gap), ctx, "tooltip gap");
            check_pixels(&Some(*padding), ctx, "tooltip padding");
            assert!(*delay_ms <= 60_000);
            check_color(&style.background, ctx);
            check_color(&style.text, ctx);
            check_color(&style.shadow.color, ctx);
            check_border(&style.border, ctx);
            check_pixels(&style.shadow.blur, ctx, "tooltip blur");
            for value in [style.shadow.x, style.shadow.y].into_iter().flatten() {
                assert!(value.is_finite() && value.abs() <= PIXEL_BOUND);
            }
            assert!(children.len() <= 2);
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Linear {
            max_width,
            wrap,
            spacing,
            padding,
            width,
            height,
            background,
            border,
            children,
            ..
        } => {
            if let Some(value) = max_width {
                assert!(value.is_finite() && (0.0..=PIXEL_BOUND).contains(value));
            }
            if let Some(Wrap {
                spacing: Some(gap), ..
            }) = wrap
            {
                assert!(gap.is_finite() && (0.0..=PIXEL_BOUND).contains(gap));
            }
            if let Some(spacing) = spacing {
                assert!(
                    spacing.is_finite() && (0.0..=PIXEL_BOUND).contains(spacing),
                    "{ctx}: spacing {spacing} outside 0..={PIXEL_BOUND}"
                );
            }
            check_edges(padding, ctx);
            check_length(width, ctx);
            check_length(height, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Flex {
            layout,
            items,
            children,
            background,
            border,
            ..
        } => {
            check_pixels(&layout.row_gap, ctx, "row gap");
            check_pixels(&layout.column_gap, ctx, "column gap");
            check_pixels(&layout.max_width, ctx, "max width");
            check_pixels(&layout.max_height, ctx, "max height");
            check_pixels(&layout.surface_max_width, ctx, "surface max width");
            check_length(&layout.surface_width, ctx);
            check_length(&layout.surface_height, ctx);
            check_length(&layout.width, ctx);
            check_length(&layout.height, ctx);
            check_edges(&layout.padding, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            assert_eq!(items.len(), children.len(), "{ctx}: flex item cardinality");
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::KeyedColumn {
            keys: row_keys,
            spacing,
            padding,
            width,
            height,
            max_width,
            virtual_row,
            children,
            ..
        } => {
            if let Some(row_keys) = row_keys {
                assert_eq!(row_keys.len(), children.len(), "{ctx}: keyed cardinality");
            }
            check_pixels(spacing, ctx, "spacing");
            check_pixels(max_width, ctx, "max width");
            check_pixels(virtual_row, ctx, "virtual row");
            if let Some(estimate) = virtual_row {
                assert!(*estimate >= 1.0);
            }
            check_edges(padding, ctx);
            check_length(width, ctx);
            check_length(height, ctx);
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Grid {
            fluid,
            spacing,
            padding,
            width,
            height,
            aspect,
            background,
            border,
            children,
            ..
        } => {
            for (name, value) in [("fluid", fluid), ("spacing", spacing), ("aspect", aspect)] {
                if let Some(value) = value {
                    assert!(
                        value.is_finite() && (0.0..=PIXEL_BOUND).contains(value),
                        "{ctx}: grid {name} {value} outside 0..={PIXEL_BOUND}"
                    );
                }
            }
            check_edges(padding, ctx);
            check_length(width, ctx);
            check_length(height, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Sensor {
            anticipate,
            delay,
            child,
            ..
        } => {
            if let Some(anticipate) = anticipate {
                assert!(
                    anticipate.is_finite() && (0.0..=PIXEL_BOUND).contains(anticipate),
                    "{ctx}: sensor anticipate {anticipate} outside 0..={PIXEL_BOUND}"
                );
            }
            if let Some(delay) = delay {
                assert!(
                    delay.is_finite() && *delay >= 0.0,
                    "{ctx}: sensor delay {delay} is not a finite non-negative number"
                );
            }
            check_bounds(child, depth + 1, keys, svg_bytes, ctx);
        }
        Node::Scroll {
            width,
            height,
            bar_width,
            bar_margin,
            scroller_width,
            bar_spacing,
            background,
            border,
            content,
            ..
        } => {
            check_length(width, ctx);
            check_length(height, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            for (value, field) in [
                (bar_width, "bar width"),
                (bar_margin, "bar margin"),
                (scroller_width, "scroller width"),
                (bar_spacing, "bar spacing"),
            ] {
                check_pixels(value, ctx, field);
            }
            check_bounds(content, depth + 1, keys, svg_bytes, ctx);
        }
        Node::MouseArea { content, .. } | Node::ResizeHandle { content, .. } => {
            check_bounds(content, depth + 1, keys, svg_bytes, ctx);
        }
        Node::Qr { code, .. } => {
            if let Some(payload) = &code.payload {
                assert!(payload.len() <= view_wire::MAX_QR_PAYLOAD_BYTES);
            }
            check_color(&code.cell, ctx);
            check_color(&code.background, ctx);
            if let Some(view_wire::QrSize::Cell(value) | view_wire::QrSize::Total(value)) =
                code.size
            {
                check_pixels(&Some(value), ctx, "QR size");
            }
        }
        Node::RichText {
            spans,
            size,
            color,
            width,
            ..
        } => {
            check_pixels(size, ctx, "rich text size");
            check_color(color, ctx);
            check_length(width, ctx);
            for span in spans {
                check_string(&span.content, ctx, "span content");
                if let Some(link) = &span.link {
                    check_string(link, ctx, "span link");
                }
                check_pixels(&span.size, ctx, "span size");
                check_color(&span.color, ctx);
                check_color(&span.background, ctx);
                check_edges(&span.padding, ctx);
                check_border(&span.border, ctx);
            }
        }
        Node::Text {
            content,
            size,
            color,
            width,
            ..
        } => {
            check_string(content, ctx, "text content");
            if let Some(size) = size {
                assert!(
                    size.is_finite() && (0.0..=TEXT_PIXEL_BOUND).contains(size),
                    "{ctx}: text size {size} outside 0..={TEXT_PIXEL_BOUND}"
                );
            }
            check_color(color, ctx);
            check_length(width, ctx);
        }
        Node::ImageViewer {
            data,
            label,
            width,
            height,
            options,
            ..
        } => {
            if let Some(data) = data {
                *svg_bytes += data.byte_len();
                assert!(data.valid_rgba(), "{ctx}: invalid viewer RGBA");
            }
            if let Some(label) = label {
                check_string(label, ctx, "viewer label");
            }
            check_length(width, ctx);
            check_length(height, ctx);
            if let Some(padding) = options.padding {
                assert!(padding.is_finite() && (0.0..=PIXEL_BOUND).contains(&padding));
            }
            if let Some((min, max)) = options.scale_bounds {
                assert!(min.is_finite() && max.is_finite() && min > 0.0 && max >= min);
            }
            if let Some(step) = options.scale_step {
                assert!(step.is_finite() && step > 0.0);
            }
        }
        Node::Image {
            data,
            label,
            rotation,
            opacity,
            width,
            height,
            ..
        } => {
            if let Some(data) = data {
                *svg_bytes += data.byte_len();
                assert!(data.valid_rgba(), "{ctx}: invalid RGBA");
            }
            if let Some(label) = label {
                check_string(label, ctx, "image label");
            }
            if let Some(Rotation::Floating(radians) | Rotation::Solid(radians)) = rotation {
                check_finite(*radians, ctx, "image rotation");
            }
            if let Some(opacity) = opacity {
                assert!(opacity.is_finite() && (0.0..=1.0).contains(opacity));
            }
            check_length(width, ctx);
            check_length(height, ctx);
        }
        Node::Svg {
            bytes,
            label,
            color,
            hover,
            rotation,
            opacity,
            width,
            height,
            ..
        } => {
            *svg_bytes += bytes.as_ref().map_or(0, Vec::len);
            if let Some(label) = label {
                check_string(label, ctx, "picture label");
            }
            check_color(color, ctx);
            if let Some(hover) = hover {
                check_color(hover, ctx);
            }
            if let Some(Rotation::Floating(radians) | Rotation::Solid(radians)) = rotation {
                check_finite(*radians, ctx, "rotation");
            }
            if let Some(opacity) = opacity {
                assert!(
                    opacity.is_finite() && (0.0..=1.0).contains(opacity),
                    "{ctx}: opacity {opacity} outside 0..=1"
                );
            }
            check_length(width, ctx);
            check_length(height, ctx);
        }
        Node::Input {
            options,
            placeholder,
            value,
            width,
            style,
            ..
        } => {
            check_string(placeholder, ctx, "placeholder");
            check_string(value, ctx, "input value");
            check_length(width, ctx);
            check_string(&options.label, ctx, "input label");
            if let Some(value) = &options.description {
                check_string(value, ctx, "input description");
            }
            check_edges(&options.padding, ctx);
            if let Some(value) = options.text_size {
                assert!(value.is_finite() && (f32::EPSILON..=TEXT_PIXEL_BOUND).contains(&value));
            }
            if let Some(value) = options.line_height {
                assert!(value.is_finite() && (f32::EPSILON..=16.0).contains(&value));
            }
            if let Some(NamedFont {
                family: FontFamily::Named(name),
                ..
            }) = &options.font
            {
                check_string(name, ctx, "input font");
            }
            check_color(&style.focus_border, ctx);
            for face in [
                Some(&style.utility),
                Some(&style.active),
                style.hovered.as_ref(),
                style.focused.as_ref(),
                style.focused_hovered.as_ref(),
                style.disabled.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                check_color(&face.background, ctx);
                check_border(&face.border, ctx);
                check_color(&face.value, ctx);
                check_color(&face.placeholder, ctx);
                check_color(&face.selection, ctx);
            }
        }
        Node::Button {
            content,
            label,
            description,
            width,
            height,
            padding,
            style,
            ..
        } => {
            match content {
                ButtonContent::Label(text) => check_string(text, ctx, "button label"),
                ButtonContent::Child(child) => check_bounds(child, depth + 1, keys, svg_bytes, ctx),
            }
            if let Some(label) = label {
                check_string(label, ctx, "accessible label");
            }
            if let Some(description) = description {
                check_string(description, ctx, "button description");
            }
            check_length(width, ctx);
            check_length(height, ctx);
            check_edges(padding, ctx);
            if let Some(recipe) = &style.recipe {
                for value in [
                    &recipe.base.background,
                    &recipe.base.text,
                    &recipe.hover_background,
                    &recipe.pressed_background,
                    &recipe.disabled_background,
                    &recipe.disabled_text,
                    &recipe.focus_ring,
                ] {
                    check_color(value, ctx);
                }
                check_border(&recipe.base.border, ctx);
                if let Some(value) = recipe.disabled_opacity {
                    assert!(value.is_finite() && (0.0..=1.0).contains(&value));
                }
                if let Some(value) = recipe.text_size {
                    assert!(value.is_finite() && (0.0..=TEXT_PIXEL_BOUND).contains(&value));
                }
                if let Some(value) = recipe.line_height {
                    assert!(value.is_finite() && (f32::EPSILON..=16.0).contains(&value));
                }
                if let Some(NamedFont {
                    family: FontFamily::Named(name),
                    ..
                }) = &recipe.font
                {
                    check_string(name, ctx, "button font");
                }
            }
            for face in [
                Some(&style.active),
                style.hovered.as_ref(),
                style.pressed.as_ref(),
                style.disabled.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                check_color(&face.background, ctx);
                check_color(&face.text, ctx);
                check_border(&face.border, ctx);
            }
        }
        Node::Space { width, height } => {
            check_length(width, ctx);
            check_length(height, ctx);
        }
        Node::Rule {
            thickness,
            color,
            radius,
            ..
        } => {
            assert!(
                thickness.is_finite() && (0.0..=PIXEL_BOUND).contains(thickness),
                "{ctx}: rule thickness {thickness} outside 0..={PIXEL_BOUND}"
            );
            check_color(color, ctx);
            for corner in radius.iter().flatten() {
                check_pixels(&Some(*corner), ctx, "rule radius");
            }
        }
        Node::Toggle {
            label,
            width,
            style,
            ..
        } => {
            check_string(label, ctx, "control label");
            check_length(width, ctx);
            for face in [
                &style.active_on,
                &style.active_off,
                &style.hovered_on,
                &style.hovered_off,
                &style.disabled_on,
                &style.disabled_off,
            ] {
                check_control_face(face, ctx);
            }
        }
        Node::Radio {
            label,
            width,
            style,
            ..
        } => {
            check_string(label, ctx, "control label");
            check_length(width, ctx);
            for face in [
                &style.active_on,
                &style.active_off,
                &style.hovered_on,
                &style.hovered_off,
            ] {
                check_control_face(face, ctx);
            }
        }
        Node::Slider {
            value,
            min,
            max,
            step,
            width,
            height,
            style,
            ..
        } => {
            for (number, field) in [(value, "value"), (min, "min"), (max, "max"), (step, "step")] {
                check_finite(*number, ctx, field);
            }
            check_length(width, ctx);
            check_length(height, ctx);
            for face in [&style.active, &style.hovered, &style.dragged] {
                check_slider_face(face, ctx);
            }
        }
        Node::ComboBox {
            state_key,
            options,
            selected,
            placeholder,
            width,
            settings,
            ..
        } => {
            check_string(state_key, ctx, "combo state identity");
            check_string(placeholder, ctx, "combo placeholder");
            assert!(options.len() <= MAX_OPTIONS, "{ctx}: combo option budget");
            for option in options {
                check_string(option, ctx, "combo option");
            }
            if let Some(index) = selected {
                assert!((*index as usize) < options.len());
            }
            check_length(width, ctx);
            check_length(&settings.menu_height, ctx);
            for value in [
                settings.padding,
                settings.icon.as_ref().map(|icon| icon.spacing),
            ]
            .into_iter()
            .flatten()
            {
                assert!(value.is_finite() && (0.0..=PIXEL_BOUND).contains(&value));
            }
            for face in [
                Some(&settings.style.utility),
                Some(&settings.style.active),
                settings.style.hovered.as_ref(),
                settings.style.focused.as_ref(),
                settings.style.focused_hovered.as_ref(),
                settings.style.disabled.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                check_color(&face.background, ctx);
                check_border(&face.border, ctx);
                check_color(&face.value, ctx);
                check_color(&face.placeholder, ctx);
                check_color(&face.selection, ctx);
                check_color(&face.icon, ctx);
            }
        }
        Node::PickList {
            options,
            selected,
            placeholder,
            width,
            style,
            ..
        } => {
            for face in [
                &style.active,
                &style.hovered,
                &style.opened,
                &style.opened_hovered,
            ] {
                check_pick_face(face, ctx);
            }
            if let Some(menu) = &style.menu {
                check_color(&menu.background, ctx);
                check_color(&menu.text, ctx);
                check_border(&menu.border, ctx);
                check_color(&menu.selected_text, ctx);
                check_color(&menu.selected_background, ctx);
            }
            assert!(
                options.len() <= MAX_OPTIONS,
                "{ctx}: {} options, over MAX_OPTIONS",
                options.len()
            );
            for option in options {
                check_string(option, ctx, "option");
            }
            if let Some(index) = selected {
                assert!(
                    (*index as usize) < options.len(),
                    "{ctx}: selected option {index} past {} options",
                    options.len()
                );
            }
            if let Some(placeholder) = placeholder {
                check_string(placeholder, ctx, "placeholder");
            }
            check_length(width, ctx);
        }
        Node::Progress {
            value,
            min,
            max,
            length,
            girth,
            background,
            bar,
            border,
            ..
        } => {
            for (number, field) in [(value, "value"), (min, "min"), (max, "max")] {
                check_finite(*number, ctx, field);
            }
            check_length(length, ctx);
            check_length(girth, ctx);
            check_color(background, ctx);
            check_color(bar, ctx);
            check_border(border, ctx);
        }
        Node::Stack {
            width,
            height,
            padding,
            background,
            border,
            children,
            under,
            ..
        } => {
            check_length(width, ctx);
            check_length(height, ctx);
            check_edges(padding, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            assert!(*under <= MAX_NODES as u32, "{ctx}: under budget");
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Hover {
            width,
            height,
            padding,
            background,
            border,
            children,
            tint,
            radius,
            ..
        } => {
            check_length(width, ctx);
            check_length(height, ctx);
            check_edges(padding, ctx);
            check_color(background, ctx);
            check_border(border, ctx);
            check_color(tint, ctx);
            assert!(
                radius.is_finite() && (0.0..=PIXEL_BOUND).contains(radius),
                "{ctx}: hover radius"
            );
            assert!(children.len() <= 2, "{ctx}: hover child count");
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Overlay {
            padding,
            backdrop,
            children,
            ..
        } => {
            assert!(
                padding.is_finite() && (0.0..=PIXEL_BOUND).contains(padding),
                "{ctx}: overlay padding"
            );
            check_color(&Some(*backdrop), ctx);
            assert!(children.len() <= 2, "{ctx}: overlay child count");
            for child in children {
                check_bounds(child, depth + 1, keys, svg_bytes, ctx);
            }
        }
        Node::Lazy { content, .. } => check_bounds(content, depth + 1, keys, svg_bytes, ctx),
        Node::Responsive { width, height, .. } => {
            check_length(width, ctx);
            check_length(height, ctx);
        }
        Node::When { condition, .. } => {
            assert!(
                condition.ops.len() <= view_wire::MAX_QUERY_OPS,
                "{ctx}: condition budget"
            );
        }
        Node::Canvas {
            width,
            height,
            commands,
            ..
        } => {
            check_length(width, ctx);
            check_length(height, ctx);
            assert!(
                commands.len() <= view_wire::MAX_CANVAS_PARTS,
                "{ctx}: canvas command budget"
            );
        }
        Node::Surface { name, args, .. } => {
            check_string(name, ctx, "surface name");
            for value in args {
                match value {
                    view_wire::SurfaceValue::Str(text) => check_string(text, ctx, "surface arg"),
                    view_wire::SurfaceValue::F64(number) => assert!(number.is_finite()),
                    _ => {}
                }
            }
        }
        Node::Editor {
            options,
            placeholder,
            document,
            width,
            height,
            min_height,
            max_height,
            ..
        } => {
            check_pixels(&options.padding, ctx, "editor padding");
            if let Some(size) = options.size {
                assert!(size.is_finite() && size > 0.0 && size <= TEXT_PIXEL_BOUND);
            }
            if let Some(line_height) = options.line_height {
                let (value, max) = match line_height {
                    LineHeight::Relative(v) => (v, PIXEL_BOUND / TEXT_PIXEL_BOUND),
                    LineHeight::Absolute(v) => (v, PIXEL_BOUND),
                };
                assert!(value.is_finite() && value > 0.0 && value <= max);
            }
            if let Some(NamedFont {
                family: FontFamily::Named(name),
                ..
            }) = &options.font
            {
                check_string(name, ctx, "editor font");
            }
            for face in [
                Some(&options.style.utility),
                Some(&options.style.active),
                options.style.hovered.as_ref(),
                options.style.focused.as_ref(),
                options.style.focused_hovered.as_ref(),
                options.style.disabled.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                check_color(&face.background, ctx);
                check_border(&face.border, ctx);
                check_color(&face.value, ctx);
                check_color(&face.placeholder, ctx);
                check_color(&face.selection, ctx);
            }
            check_string(placeholder, ctx, "editor placeholder");
            // A document is metadata: sanitize keeps a valid reference whole,
            // and never spends the display budget on the bytes it names.
            assert_eq!(
                document.validate(),
                Ok(()),
                "{ctx}: sanitize kept an invalid editor document reference"
            );
            check_length(height, ctx);
            for value in [width, min_height, max_height].into_iter().flatten() {
                assert!(
                    value.is_finite() && (0.0..=PIXEL_BOUND).contains(value),
                    "{ctx}: editor size {value} outside 0..={PIXEL_BOUND}"
                );
            }
        }
    }
}

/// Every post-condition `sanitize` promises about a whole frame: the tree's
/// node count and every bound `check_bounds` covers, plus every request's
/// `kind`.
/// Every editor document reference in the tree, in one fixed walk order, so
/// the same tree before and after `sanitize` compares element for element.
fn document_refs(root: &Node) -> Vec<editor_document::EditorDocumentRef> {
    let mut pending = vec![root];
    let mut references = Vec::new();
    while let Some(node) = pending.pop() {
        if let Node::Editor { document, .. } = node {
            references.push(document.clone());
        }
        pending.extend(node.children());
    }
    references
}

fn check_frame(frame: &Frame, ctx: &str) {
    if let Some(root) = &frame.root {
        assert!(
            root.count() <= MAX_NODES,
            "{ctx}: {} nodes, over MAX_NODES",
            root.count()
        );
        let mut keys = HashSet::new();
        let mut svg_bytes = 0;
        check_bounds(root, 0, &mut keys, &mut svg_bytes, ctx);
        assert!(
            svg_bytes <= MAX_PICTURE_BYTES_PER_FRAME,
            "{ctx}: {svg_bytes} picture bytes, over MAX_PICTURE_BYTES_PER_FRAME"
        );
    }
    for request in &frame.requests {
        check_string(&request.kind, ctx, "request kind");
    }
}

// --------------------------------------------------------------- test 1

/// Random trees, decoded and sanitized, always land inside every bound
/// `sanitize` promises — or `decode` refused them for a reason the door
/// actually names, and the tree really was over it.
#[test]
fn random_trees_come_out_of_sanitize_inside_every_bound() {
    // The task asked for ~300; without decode's own recursion needing a
    // dedicated thread (its depth door caps recursion at MAX_DEPTH, safe on
    // a normal stack — see `on_big_stack`'s doc comment), 200 trees with a
    // steep width/depth skew keep this test's slice of the file's ~10s
    // debug budget comfortably small.
    const SEED: u64 = 0x5EED_F00D_1234_5678;
    const NUM_TREES: usize = 200;

    for i in 0..NUM_TREES {
        let seed = SEED ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let ctx = format!("seed={seed:#x} tree={i}");
        let (frame, bytes) = build_and_encode(seed, i);

        match decode::<Frame>(&bytes) {
            Err(message) => {
                let root = frame.root.as_ref().expect("gen_frame always sets a root");
                let depth_over = tree_depth(root) > MAX_DEPTH;
                let count_over = root.count() > MAX_DECODED_NODES + 1;
                assert!(
                    depth_over || count_over,
                    "{ctx}: decode refused a tree that was not actually over either \
                     door (depth {}, nodes {}): {message}",
                    tree_depth(root),
                    root.count()
                );
                let names_the_door = message.contains("deeper than the host renders")
                    || message.contains("more nodes than the host holds");
                assert!(names_the_door, "{ctx}: unexpected refusal: {message}");
            }
            Ok(mut decoded) => {
                let before = decoded.root.as_ref().map(document_refs).unwrap_or_default();
                match sanitize(&mut decoded) {
                    Ok(_) => {
                        check_frame(&decoded, &ctx);
                        let after = decoded.root.as_ref().map(document_refs).unwrap_or_default();
                        assert_eq!(
                            before, after,
                            "{ctx}: sanitize rewrote an editor document reference"
                        );
                    }
                    // The only frame sanitize refuses is one whose editor
                    // documents it could not keep whole; every other bound is
                    // pulled into range instead.
                    Err(refused) => assert!(
                        [
                            "invalid editor document references or budget",
                            "frame budget would remove an editor document projection",
                        ]
                        .contains(&refused),
                        "{ctx}: unexpected refusal: {refused}"
                    ),
                }
            }
        }
    }
}

// --------------------------------------------------------------- test 2

fn corrupt_length_prefix(rng: &mut Rng, bytes: &mut [u8]) {
    if bytes.len() < 8 {
        return;
    }
    let at = rng.next_range(bytes.len() - 7);
    let value = *rng.choose(&[u64::MAX, 1u64 << 40, 0u64]);
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// One random mutation applied to a copy of a sound frame's bytes: a bit
/// flip, a byte overwrite, a truncation, an insertion of random bytes, or a
/// length-prefix corruption (an 8-byte little-endian window overwritten
/// with a value a real `Vec`/`String` length prefix would never hold).
fn mutate_once(rng: &mut Rng, bytes: &mut Vec<u8>) {
    if bytes.is_empty() {
        bytes.push(rng.next_range(256) as u8);
        return;
    }
    match rng.next_range(5) {
        0 => {
            let i = rng.next_range(bytes.len());
            bytes[i] ^= 1 << rng.next_range(8);
        }
        1 => {
            let i = rng.next_range(bytes.len());
            bytes[i] = rng.next_range(256) as u8;
        }
        2 => {
            let cut = rng.next_range(bytes.len() + 1);
            bytes.truncate(cut);
        }
        3 => {
            let at = rng.next_range(bytes.len() + 1);
            let junk: Vec<u8> = (0..1 + rng.next_range(16))
                .map(|_| rng.next_range(256) as u8)
                .collect();
            bytes.splice(at..at, junk);
        }
        _ => corrupt_length_prefix(rng, bytes),
    }
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Bytes a hostile guest could have written — a sound frame with random
/// bit flips, byte overwrites, truncations, insertions, and corrupted
/// length prefixes — never make `decode` panic. `decode`'s own depth door
/// (checked before each level is even built) is what makes this safe on a
/// plain stack: see `lib.rs`'s `bytes_a_hostile_guest_could_write_are_answered_not_survived`,
/// which this test generalizes to frames far larger than a single flipped
/// bit's worth of hand-written cases.
#[test]
fn mutated_bytes_never_panic() {
    const SEED: u64 = 0xBADF_00D5_A5A5_5A5A;
    const NUM_FRAMES: usize = 50;
    const MUTATIONS_PER_FRAME: usize = 200;

    for i in 0..NUM_FRAMES {
        let seed = SEED ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let (_frame, bytes) = build_and_encode_bounded(seed);
        let mut mutator = Rng::new(seed ^ 0xF00D);

        for m in 0..MUTATIONS_PER_FRAME {
            let mut mutated = bytes.clone();
            mutate_once(&mut mutator, &mut mutated);
            let ctx = format!("seed={seed:#x} frame={i} mutation={m}");

            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode::<Frame>(&mutated)
            }));
            match outcome {
                Ok(Ok(mut decoded)) => {
                    if sanitize(&mut decoded).is_ok() {
                        check_frame(&decoded, &ctx);
                    }
                }
                Ok(Err(_)) => {}
                Err(payload) => panic!("{ctx}: decode panicked: {}", payload_message(&payload)),
            }
        }
    }
}

// --------------------------------------------------------------- test 2b

/// A sanitized tree, patched by any sequence the wire decodes, is a
/// sanitized tree or a refusal: every bound `check_bounds` covers holds of
/// what `apply` returns `Ok` on, whatever the patches inserted, replaced or
/// shuffled — including subtrees over every ceiling on their own, and keys
/// the tree already holds. A refusal names one of the doors `apply` has.
#[test]
fn a_patched_sanitized_tree_is_a_sanitized_tree() {
    const SEED: u64 = 0x9A7C_4E5D_0B1A_2F3E;
    const NUM_TREES: usize = 60;
    const PATCHES_PER_TREE: usize = 24;

    for i in 0..NUM_TREES {
        let seed = SEED ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let ctx = format!("seed={seed:#x} tree={i}");
        on_big_stack(move || {
            let mut rng = Rng::new(seed);
            let (depth, width) = (rng.skewed(MAX_DEPTH, 3), rng.skewed(64, 2));
            let mut frame = gen_frame_with(&mut rng, depth, width);
            if sanitize(&mut frame).is_err() {
                return;
            }
            let mut root = frame
                .root
                .take()
                .expect("gen_frame_with always sets a root");
            let hostile = rng.next_range(3) == 0;
            let mut patches = Vec::new();
            let mut staged = root.clone();
            for _ in 0..rng.next_range(PATCHES_PER_TREE + 1) {
                let patch = gen_patch(&mut rng, &staged, hostile);
                // Paths are drawn against the tree as the patches so far
                // leave it, so a well-behaved sequence applies whole and
                // a hostile one is refused somewhere along it.
                let mut candidate = staged.clone();
                let applied = view_wire::apply(&mut candidate, vec![patch.clone()]);
                if matches!(
                    applied,
                    Err("invalid editor document references or budget"
                        | "frame budget would remove an editor document projection")
                ) {
                    continue;
                }
                staged = candidate;
                assert!(
                    hostile || applied.is_ok(),
                    "{ctx}: a well-behaved patch was refused: {applied:?}\n{patch:#?}"
                );
                patches.push(patch);
            }
            let patched = Frame {
                patches,
                ..Frame::default()
            };
            // A patch's subtree meets the same door a root does: one nested
            // past what the host walks is refused before it is built.
            let decoded: Frame = match decode(&encode(&patched)) {
                Ok(decoded) => decoded,
                Err(message) => {
                    assert!(
                        message.contains("deeper than the host renders")
                            || message.contains("more nodes than the host holds"),
                        "{ctx}: unexpected refusal: {message}"
                    );
                    return;
                }
            };
            let outcome = view_wire::apply(&mut root, decoded.patches);
            assert!(
                hostile
                    || outcome.is_ok()
                    || matches!(
                        outcome,
                        Err("invalid editor document references or budget"
                            | "frame budget would remove an editor document projection")
                    ),
                "{ctx}: a structurally valid sequence was refused: {outcome:?}"
            );
            match outcome {
                Ok(_) => {
                    let checked = Frame {
                        root: Some(root),
                        ..Frame::default()
                    };
                    check_frame(&checked, &ctx);
                }
                Err(refused) => {
                    let named = [
                        "a path to no node",
                        "an index past the list",
                        "a list edit on no list",
                        "props of another arity",
                        "more patches than the host applies",
                        "invalid editor document references or budget",
                        "frame budget would remove an editor document projection",
                    ]
                    .contains(&refused);
                    assert!(named, "{ctx}: unexpected refusal: {refused}");
                }
            }
        });
    }
}

// --------------------------------------------------------------- test 2c

/// `diff` then `apply` is the identity on the new tree, and leaves both
/// inputs as they were. The new tree is the old one with a handful of
/// well-behaved edits applied — so the pair shares most of its structure
/// and the diff has to find moves, inserts, removes and field changes
/// inside lists whose keys come from a five-entry pool — and, one time in
/// eight, an unrelated tree, which is a `Replace` at the root. A pair whose
/// diff runs past `MAX_PATCHES` is the guest's cue to send the tree whole,
/// so it is only checked for that refusal.
#[test]
fn a_diff_applied_to_the_old_tree_is_the_new_tree_for_random_pairs() {
    const SEED: u64 = 0xD1FF_0000_A99B_1E5A;
    const NUM_PAIRS: usize = 150;
    const EDITS_PER_PAIR: usize = 8;

    for i in 0..NUM_PAIRS {
        let seed = SEED ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let ctx = format!("seed={seed:#x} pair={i}");
        on_big_stack(move || {
            let mut rng = Rng::new(seed);
            let tree = |rng: &mut Rng| {
                let (depth, width) = (rng.skewed(MAX_DEPTH / 2, 3), rng.skewed(48, 2));
                let mut frame = gen_frame_with(rng, depth, width);
                sanitize(&mut frame).ok()?;
                frame.root.take()
            };
            let Some(mut old) = tree(&mut rng) else {
                return;
            };
            let mut new = match rng.next_range(8) {
                0 => {
                    let Some(tree) = tree(&mut rng) else {
                        return;
                    };
                    tree
                }
                _ => {
                    let mut edited = old.clone();
                    for _ in 0..1 + rng.next_range(EDITS_PER_PAIR) {
                        let patch = gen_patch(&mut rng, &edited, false);
                        let mut candidate = edited.clone();
                        match view_wire::apply(&mut candidate, vec![patch]) {
                            Ok(_) => edited = candidate,
                            Err(
                                "invalid editor document references or budget"
                                | "frame budget would remove an editor document projection",
                            ) => {}
                            Err(refused) => panic!("{ctx}: {refused}"),
                        }
                    }
                    edited
                }
            };
            let (old_before, new_before) = (old.clone(), new.clone());
            let patches = diff(&mut old, &mut new);
            assert_eq!(old, old_before, "{ctx}: diff moved the old tree");
            assert_eq!(new, new_before, "{ctx}: diff moved the new tree");
            let count = patches.len();
            let mut applied = old;
            match view_wire::apply(&mut applied, patches) {
                Ok(_) => assert_eq!(applied, new, "{ctx}: {count} patches"),
                Err(refused) => assert!(
                    count > MAX_PATCHES && refused == "more patches than the host applies",
                    "{ctx}: {count} patches refused: {refused}"
                ),
            }
        });
    }
}

// --------------------------------------------------------------- test 3

/// A hand-crafted length-prefix bomb — a `Frame` whose root is a `Linear`
/// claiming `2^40` children, with the buffer cut off a few bytes later — is
/// refused without decode trying to build any of it.
///
/// The layout is worked out rather than hand-counted: encoding a `Linear`
/// with zero children and one with a single child differ only in the
/// 8-byte little-endian length prefix bincode writes ahead of a `Vec`'s
/// elements (everything before it — the enum discriminant, the key string,
/// the `Option` tags for `spacing`/`padding`/`width`/`height`/`align` — is
/// byte-for-byte identical either way, and the first divergent byte is
/// that prefix's low byte, 0x00 vs 0x01). Taking the common prefix length
/// of the two encodings finds that offset without hard-coding it.
#[test]
fn a_length_prefix_bomb_is_refused_without_the_allocation() {
    fn linear(children: Vec<Node>) -> Frame {
        Frame {
            root: Some(Node::Linear {
                max_width: None,
                clip: false,
                wrap: None,
                key: "k".into(),
                axis: Axis::Column,
                spacing: None,
                padding: None,
                width: None,
                height: None,
                align: None,
                background: None,
                border: None,
                children,
            }),
            ..Frame::default()
        }
    }
    let empty_children = linear(vec![]);
    let one_child = linear(vec![Node::empty()]);

    let bytes_empty = encode(&empty_children);
    let bytes_one = encode(&one_child);
    let offset = bytes_empty
        .iter()
        .zip(bytes_one.iter())
        .take_while(|(a, b)| a == b)
        .count();
    assert!(
        offset > 0 && offset + 8 <= bytes_empty.len(),
        "could not locate the children length prefix (offset {offset})"
    );

    let mut bomb = bytes_empty[..offset].to_vec();
    bomb.extend_from_slice(&(1u64 << 40).to_le_bytes());
    // The buffer ends a handful of bytes after the claimed count — nowhere
    // near what 2^40 elements would take — which is the whole point: a
    // decoder that trusted the prefix enough to preallocate would already
    // have tried and failed before it noticed.
    bomb.extend_from_slice(&[0u8; 4]);

    let start = std::time::Instant::now();
    let result = decode::<Frame>(&bomb);
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "a 2^40-child claim with no data was accepted"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "refusing the bomb took {elapsed:?}, which means something tried to act on the claimed count"
    );
}

// --------------------------------------------------------------- test 4

/// `sanitize` is idempotent: running it again on its own output changes
/// nothing, which is what lets a host call it on every frame without
/// worrying whether the guest already sent a clean one.
#[test]
fn sanitize_is_idempotent() {
    const SEED: u64 = 0x1DE4_1DE4_5EED_5EED;
    const NUM_TREES: usize = 150;

    for i in 0..NUM_TREES {
        let seed = SEED ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let ctx = format!("seed={seed:#x} tree={i}");
        let mut once = build_frame(seed, i);
        if sanitize(&mut once).is_err() {
            continue;
        }
        let mut twice = once.clone();
        sanitize(&mut twice).unwrap();
        assert_eq!(once, twice, "{ctx}: sanitize is not idempotent");
    }
}

#[test]
fn resize_handle_round_trip_retains_routes_and_checks_its_child() {
    let mut frame = gen_frame_with(&mut Rng::new(17), 0, 0);
    frame.root = Some(Node::ResizeHandle {
        key: "divider".into(),
        on_press: Some(1),
        on_release: Some(2),
        on_drag: Some(u32::MAX),
        cursor: Some(mouse::Cursor::ResizingHorizontally),
        content: Box::new(Node::Space {
            width: Some(Length::Fixed(f32::INFINITY)),
            height: Some(Length::Fixed(-10.0)),
        }),
    });
    assert_eq!(tree_depth(frame.root.as_ref().unwrap()), 1);
    let mut decoded: Frame = decode(&encode(&frame)).unwrap();
    sanitize(&mut decoded).unwrap();
    check_frame(&decoded, "resize child");
    let Node::ResizeHandle {
        on_press,
        on_release,
        on_drag,
        cursor,
        ..
    } = decoded.root.unwrap()
    else {
        panic!("resize handle retained");
    };
    assert_eq!(
        (on_press, on_release, on_drag),
        (Some(1), Some(2), Some(u32::MAX))
    );
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
}
