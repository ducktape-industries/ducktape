//! The shipping platform shaper must keep bundled Latin faces and bounded fallback work.
use crate::frame_probe::{FRAMES, Phase, headless_context};
use gpui_kit::{FontWeight, TextRun, WindowTextSystem, font, px};
const WEIGHTS: [FontWeight; 4] = [
    FontWeight::NORMAL,
    FontWeight::MEDIUM,
    FontWeight::SEMIBOLD,
    FontWeight::BOLD,
];
fn shape(system: &WindowTextSystem, content: &str, weight: FontWeight) -> gpui_kit::ShapedLine {
    let mut face = font("Geist");
    face.weight = weight;
    // What the app draws with: the shell puts this chain on the root text
    // style, and nothing below it replaces the font wholesale.
    face.fallbacks = Some(crate::shell::fallback_chain());
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
                let shaped = phase.sample(|| shape(&shaper, &text, weight));
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
fn latin_text_at_every_weight_is_shaped_with_geist() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    let mut widths = Vec::new();
    for weight in WEIGHTS {
        let line = shape(&shaper, "Channel", weight);
        assert!(!line.runs.is_empty());
        for run in &line.runs {
            let face = cx
                .text_system()
                .get_font_for_id(run.font_id)
                .expect("requested font was resolved");
            assert_eq!(face.family.as_ref(), "Geist");
        }
        widths.push(line.width());
    }
    // THE FAMILY CANNOT TELL A WEIGHT APART. The text system keeps the
    // requested weight only long enough to match a face and then shapes with
    // that face's own declared weight, so a family holding one variable face
    // answers every request with the family name that was asked for and the
    // advances of weight 400 — NORMAL and BOLD both measured 50.975998px.
    // Only the advances prove a second face was matched.
    let [regular, _medium, semibold, bold] = widths[..] else {
        panic!("one width per weight");
    };
    assert_ne!(
        regular, bold,
        "NORMAL and BOLD shape identically — the family answers both with one face",
    );
    // AND SEMIBOLD IS DRAWN AT THE BOLD FACE, because the registered set is
    // the two RIBBI weights and a request lands on the nearer of them. The
    // substitution above runs the other way too — the matched face's weight
    // is the weight every FALLBACK lookup runs at — which is what
    // `shell::FALLBACK_FAMILIES` bounds: with an explicit chain a span the
    // Latin face cannot cover is tagged with the family that covers it
    // instead of sending the shaper off to walk the font database.
    assert_eq!(
        semibold, bold,
        "SEMIBOLD no longer lands on the Bold face — the registered Latin set \
         is no longer the two RIBBI weights",
    );
}

#[test]
fn resolving_the_fallback_chain_leaves_the_emoji_face_in_the_database() {
    let cx = headless_context();
    let shaper = WindowTextSystem::new(cx.text_system().clone());
    // Shaping through `fallback_chain()` is what resolves every family in it.
    shape(&shaper, "한글", FontWeight::NORMAL);
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
