//! Copied runs in a single host-owned rich text paragraph.
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RichSpan {
    pub content: String,
    pub size: Option<f32>,
    pub line_height: Option<LineHeight>,
    pub font: Option<NamedFont>,
    pub color: Option<Rgba>,
    pub link: Option<String>,
    pub background: Option<Rgba>,
    pub border: Option<Border>,
    pub padding: Option<Edges>,
    pub underline: bool,
    pub strikethrough: bool,
}

pub(super) fn sanitize(spans: &mut Vec<RichSpan>, text: &mut usize, nodes: &mut usize) {
    spans.truncate(*nodes);
    *nodes = nodes.saturating_sub(spans.len());
    for span in spans {
        spend_text(&mut span.content, text);
        if let Some(link) = &mut span.link {
            spend_text(link, text);
        }
        let mut options = TextOptions {
            line_height: span.line_height,
            font: span.font.take(),
            ..Default::default()
        };
        options.sanitize(text);
        span.line_height = options.line_height;
        span.font = options.font;
        if let Some(size) = &mut span.size {
            *size = bounded(*size).min(MAX_TEXT_PIXELS);
        }
        bound_color(&mut span.color);
        bound_color(&mut span.background);
        bound_border(&mut span.border);
        bound_edges(&mut span.padding);
    }
}

pub(super) fn decode_spans<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<RichSpan>, D::Error> {
    struct Spans;
    impl<'de> serde::de::Visitor<'de> for Spans {
        type Value = Vec<RichSpan>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded rich text spans")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            if seq
                .size_hint()
                .is_some_and(|count| count > MAX_DECODED_NODES)
            {
                return Err(serde::de::Error::custom("too many rich text spans"));
            }
            let reserved = seq.size_hint();
            if let Some(count) = reserved {
                budget::spend(count).map_err(serde::de::Error::custom)?;
            }
            let mut spans = Vec::new();
            while let Some(span) = seq.next_element()? {
                if reserved.is_none() {
                    budget::spend(1).map_err(serde::de::Error::custom)?;
                }
                if spans.len() == MAX_DECODED_NODES {
                    return Err(serde::de::Error::custom("too many rich text spans"));
                }
                spans.push(span);
            }
            Ok(spans)
        }
    }
    deserializer.deserialize_seq(Spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spans_share_text_and_node_budgets_and_bound_styles() {
        let mut spans = vec![
            RichSpan {
                content: "ééé".into(),
                link: Some("target".into()),
                size: Some(f32::INFINITY),
                line_height: Some(LineHeight::Relative(f32::MAX)),
                padding: Some(Edges {
                    top: f32::NAN,
                    right: -1.0,
                    bottom: f32::INFINITY,
                    left: 2.0
                }),
                color: Some(Rgba([f32::NAN, 2.0, -1.0, 1.0])),
                ..Default::default()
            };
            3
        ];
        let (mut text, mut nodes) = (5, 2);
        sanitize(&mut spans, &mut text, &mut nodes);
        assert_eq!(spans.len(), 2);
        assert_eq!(nodes, 0);
        assert_eq!(text, 0);
        assert_eq!(spans[0].content, "éé");
        assert_eq!(spans[0].link.as_deref(), Some("t"));
        assert!(spans[1].content.is_empty());
        assert_eq!(spans[0].size, Some(MAX_TEXT_PIXELS));
        assert_eq!(
            spans[0].line_height,
            Some(LineHeight::Relative(MAX_PIXELS / MAX_TEXT_PIXELS))
        );
        assert_eq!(spans[0].color, Some(Rgba([0.0, 1.0, 0.0, 1.0])));
        let edges = spans[0].padding.unwrap();
        assert_eq!(
            (edges.top, edges.right, edges.bottom, edges.left),
            (0.0, 0.0, MAX_PIXELS, 2.0)
        );
    }
    #[test]
    fn multiple_paragraphs_share_the_decode_allowance() {
        #[derive(Debug, Deserialize)]
        struct Spans(#[serde(deserialize_with = "decode_spans")] Vec<RichSpan>);
        let count = MAX_DECODED_NODES / 2 + 1;
        let mut bytes = 2u64.to_le_bytes().to_vec();
        let payload = encode(&RichSpan::default()).repeat(count);
        for _ in 0..2 {
            bytes.extend((count as u64).to_le_bytes());
            bytes.extend(&payload);
        }
        let result = decode::<Vec<Spans>>(&bytes);
        assert!(
            matches!(result, Err(ref error) if error.contains("more nodes than the host holds")),
            "paragraphs must share their decode allowance"
        );
        let small: Spans = decode(&encode(&vec![RichSpan::default()])).unwrap();
        assert_eq!(small.0.len(), 1, "the next frame gets a fresh allowance");
    }
    #[test]
    fn span_length_prefix_is_rejected_before_reading_payloads() {
        #[derive(Debug, Deserialize)]
        struct Spans(#[serde(deserialize_with = "decode_spans")] Vec<RichSpan>);
        let one = RichSpan {
            content: "one".into(),
            ..Default::default()
        };
        let parsed: Spans = decode(&encode(&vec![one.clone()])).unwrap();
        assert_eq!(parsed.0, vec![one]);
        let error = decode::<Spans>(&(MAX_DECODED_NODES as u64 + 1).to_le_bytes()).unwrap_err();
        assert!(
            error.to_string().contains("too many rich text spans"),
            "{error}"
        );
    }
}
