//! The shipping platform shaper must keep the bundled faces — one per weight
//! AND per slant — and bounded fallback work.
use crate::frame_probe::{FRAMES, Phase, headless_context};
use gpui_kit::{Font, FontStyle, FontWeight, TextRun, WindowTextSystem, font, px};
const WEIGHTS: [FontWeight; 4] = [
    FontWeight::NORMAL,
    FontWeight::MEDIUM,
    FontWeight::SEMIBOLD,
    FontWeight::BOLD,
];
/// The body face as the app draws it: the shell puts this chain on the root
/// text style and nothing below it replaces the font wholesale.
fn body(weight: FontWeight, style: FontStyle) -> Font {
    let mut face = font(::design::fonts::FAMILY_UI);
    face.weight = weight;
    face.style = style;
    face.fallbacks = Some(crate::shell::fallback_chain());
    face
}
/// The code face as the app draws it — `shell::with_family` swaps the chain
/// with the family, so a Hangul run in a terminal or a diff reaches the
/// monospaced Hangul face instead of the proportional one.
fn code(weight: FontWeight, style: FontStyle) -> Font {
    let mut face = font(::design::fonts::FAMILY_MONO);
    face.weight = weight;
    face.style = style;
    face.fallbacks = Some(crate::shell::mono_fallback_chain());
    face
}
fn shape(system: &WindowTextSystem, content: &str, face: Font) -> gpui_kit::ShapedLine {
    system.shape_line(
        content.to_owned().into(),
        px(13.5),
        &[TextRun {
            len: content.len(),
            font: face,
            ..Default::default()
        }],
        None,
    )
}
#[test]
fn non_regular_weights_shape_at_the_regular_fallback_cost() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let cx = headless_context();
    for content in ["🎉", "♡", "한글", "Channel"] {
        let mut costs = Vec::new();
        for weight in WEIGHTS {
            // Fallback font ids can coincide across weights. Independent
            // layout caches prevent heavier weights measuring regular's hits.
            let shaper = WindowTextSystem::new(cx.text_system().clone());
            let mut phase = Phase::new("native fallback shaping");
            for index in 0..FRAMES {
                // Distinct text avoids measuring only the line-layout cache hit.
                let text = format!("{content} {index}");
                let shaped =
                    phase.sample(|| shape(&shaper, &text, body(weight, FontStyle::Normal)));
                assert!(!shaped.runs.is_empty());
                assert!(shaped.width() > px(0.));
            }
            tracing::info!(
                "fallback shaping {content} @{:<3} allocs(p50)={:>6}  {:>6}us",
                weight.0 as u32,
                phase.median_allocations(),
                phase.median_us(),
            );
            costs.push(phase.median_allocations());
        }
        assert!(costs[0] > 0, "shape work must actually be measured");
        for cost in &costs[1..] {
            assert!(
                *cost <= 2 * costs[0],
                "{content}: fallback weight costs {costs:?}"
            );
        }
    }
}
#[test]
fn a_slant_costs_what_an_upright_run_costs_in_either_family() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let cx = headless_context();
    let mut costs = Vec::new();
    for (family, face) in [("body", body as fn(_, _) -> Font), ("code", code)] {
        for weight in [FontWeight::NORMAL, FontWeight::BOLD] {
            for (slant, style) in [
                ("upright", FontStyle::Normal),
                ("italic", FontStyle::Italic),
            ] {
                for content in ["Channel", "한글"] {
                    let shaper = WindowTextSystem::new(cx.text_system().clone());
                    let mut phase = Phase::new("native fallback shaping");
                    for index in 0..FRAMES {
                        let text = format!("{content} {index}");
                        let shaped = phase.sample(|| shape(&shaper, &text, face(weight, style)));
                        assert!(!shaped.runs.is_empty());
                        assert!(shaped.width() > px(0.));
                    }
                    tracing::info!(
                        "{family:<4} {slant:<7} @{:<3} {content:<7} allocs(p50)={:>6}  {:>6}us",
                        weight.0 as u32,
                        phase.median_allocations(),
                        phase.median_us(),
                    );
                    costs.push(phase.median_allocations());
                }
            }
        }
    }
    // A SLANT IS A SECOND FACE IN THE SAME FAMILY, not a second search. It is
    // matched by `find_best_match` from the candidates the family already
    // loaded, and the Hangul faces behind both chains are upright, so an
    // italic Korean run resolves through the same chain slot as an upright
    // one. Anything dearer means a family stopped resolving and the shaper
    // went back to walking the font database.
    let baseline = costs[0];
    assert!(baseline > 0, "shape work must actually be measured");
    for cost in &costs[1..] {
        assert!(*cost <= 2 * baseline, "slant/family costs {costs:?}");
    }
}
#[test]
fn latin_text_at_every_weight_is_shaped_with_the_body_family() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    let mut widths = Vec::new();
    for weight in WEIGHTS {
        let line = shape(&shaper, "Channel", body(weight, FontStyle::Normal));
        assert!(!line.runs.is_empty());
        for run in &line.runs {
            let face = cx
                .text_system()
                .get_font_for_id(run.font_id)
                .expect("requested font was resolved");
            assert_eq!(face.family.as_ref(), ::design::fonts::FAMILY_UI);
        }
        widths.push(line.width());
    }
    // THE FAMILY NAME CANNOT TELL A WEIGHT APART. The text system keeps the
    // requested weight only long enough to match a face and then shapes with
    // that face's own declared weight, so a family holding one variable face
    // answers every request with the family name that was asked for and the
    // advances of weight 400. Only the advances prove a second face was
    // matched.
    let [regular, _medium, semibold, bold] = widths[..] else {
        panic!("one width per weight");
    };
    assert_ne!(
        regular, bold,
        "NORMAL and BOLD shape identically — the family answers both with one face",
    );
    // AND SEMIBOLD IS DRAWN AT THE BOLD FACE, because the registered set is
    // the RIBBI weights and a request lands on the nearer of them. The
    // substitution above runs the other way too — the matched face's weight
    // is the weight every FALLBACK lookup runs at — which is what
    // `shell::FALLBACK_FAMILIES` bounds: with an explicit chain a span the
    // primary face cannot cover is tagged with the family that covers it
    // instead of sending the shaper off to walk the font database.
    assert_eq!(
        semibold, bold,
        "SEMIBOLD no longer lands on the Bold face — the registered set \
         is no longer the RIBBI weights",
    );
}
#[test]
fn an_italic_request_lands_on_a_second_face_in_both_families() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    for face in [body as fn(_, _) -> Font, code] {
        for weight in [FontWeight::NORMAL, FontWeight::BOLD] {
            let upright = shape(&shaper, "Channel", face(weight, FontStyle::Normal));
            let italic = shape(&shaper, "Channel", face(weight, FontStyle::Italic));
            let requested = cx
                .text_system()
                .get_font_for_id(italic.runs[0].font_id)
                .expect("requested font was resolved");
            // THE ONLY PROOF IS A DIFFERENT FACE ID. Nothing in this stack
            // shears a glyph — `render_glyph_image` builds a swash scaler
            // with a size and a hint setting and nothing else — so a family
            // with no italic file draws an italic request upright and no
            // width, metric or family name gives it away. `find_best_match`
            // scores a style mismatch at 1000 against a weight difference of
            // at most 300, so it leaves the upright face for another one
            // ONLY when that other face is actually italic. Different ids at
            // one weight therefore mean the Italic file was matched; equal
            // ids mean it is missing and the run is drawn upright.
            //
            // Width cannot stand in for this: the code face is monospaced
            // and its italic advances are identical to its upright ones.
            assert_ne!(
                upright.runs[0].font_id, italic.runs[0].font_id,
                "{} at {} draws its italic with the upright face",
                requested.family, weight.0 as u32,
            );
        }
    }
}
#[test]
fn hangul_falls_back_to_the_bundled_face_its_family_names_and_stays_upright() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    for (face, hangul) in [
        (body as fn(_, _) -> Font, ::design::fonts::FAMILY_UI_HANGUL),
        (code, ::design::fonts::FAMILY_MONO_HANGUL),
    ] {
        let upright = shape(&shaper, "한글", face(FontWeight::NORMAL, FontStyle::Normal));
        let expected = cx.text_system().resolve_font(&font(hangul));
        // THE CHAIN, NOT THE WALK, PICKED THIS. `compute_run_spans` tags an
        // uncovered span with the first chain family that covers it, and each
        // family leads its own chain with its own Hangul face — the code
        // face's is monospaced, because a proportional one would shape 한글
        // off the column grid a terminal and a diff are drawn on.
        assert_eq!(
            upright.runs[0].font_id, expected,
            "한글 in this family did not land on {hangul}",
        );
        // NO OPEN HANGUL FONT HAS AN ITALIC. Both bundled Hangul faces are
        // upright only, so an italic run slants its Latin and leaves its
        // Korean standing — the same face id, not a second one.
        let italic = shape(&shaper, "한글", face(FontWeight::NORMAL, FontStyle::Italic));
        assert_eq!(
            italic.runs[0].font_id, expected,
            "an italic 한글 run left {hangul} for another face",
        );
    }
}
#[test]
fn resolving_the_fallback_chain_leaves_the_emoji_face_in_the_database() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    // Shaping through both chains is what resolves every family in them.
    shape(&shaper, "한글", body(FontWeight::NORMAL, FontStyle::Normal));
    shape(&shaper, "한글", code(FontWeight::NORMAL, FontStyle::Normal));
    let families = cx.text_system().all_font_names();
    // NAMING AN EMOJI FAMILY IN THE CHAIN DELETES IT. Resolving a chain entry
    // runs `load_family`, which removes from the font database any face whose
    // charmap has no 'm' — every color emoji face. Emoji then have no face at
    // all and each run pays a full database walk instead: 🎉 measured 200us
    // before and 11ms after.
    assert!(
        families.iter().any(|family| family.contains("Emoji")),
        "the emoji face is gone from the database: {families:?}",
    );
}
