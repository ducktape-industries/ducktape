//! The shipping platform shaper must keep bundled Latin faces and bounded fallback work.
use crate::frame_probe::{FRAMES, Phase, headless_context};
use gpui_kit::{FontWeight, TextRun, WindowTextSystem, font, px};
const WEIGHTS: [FontWeight; 3] = [FontWeight::NORMAL, FontWeight::SEMIBOLD, FontWeight::BOLD];
fn shape(system: &WindowTextSystem, content: &str, weight: FontWeight) -> gpui_kit::ShapedLine {
    let mut face = font("Geist");
    face.weight = weight;
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
            phase.report();
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
    let [regular, semibold, bold] = widths[..] else {
        panic!("one width per weight");
    };
    assert_ne!(
        regular, bold,
        "NORMAL and BOLD shape identically — the family answers both with one face",
    );
    // AND SEMIBOLD IS DRAWN AT THE BOLD FACE, ON PURPOSE. The substitution
    // above runs the other way too: the matched face's weight is the weight
    // every FALLBACK lookup runs at, and no system face declares 500 or 600,
    // so shipping a Medium or a SemiBold sent every non-Latin run off to walk
    // the font database — one uncached line of Korean measured 5.0ms at 500
    // and 6.0ms at 600, against 0.2ms at 400 and 700. Adding either face
    // brings that back, so this equality is the guard, not an oversight.
    assert_eq!(
        semibold, bold,
        "SEMIBOLD no longer lands on the Bold face — a 500/600 face costs \
         every non-Latin run a font-database walk",
    );
}
