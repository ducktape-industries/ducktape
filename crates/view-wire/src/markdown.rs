//! The first argument of a named markdown viewer surface.
use crate::{MAX_TEXT_PIXELS, SurfaceValue as V};

/// Resolved markdown settings, independent of a native parser or renderer.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownDocument {
    pub source: String,
    /// Background, text, primary, success, warning, danger.
    pub palette: [[f32; 4]; 6],
    /// Body, h1–h6, code size, spacing.
    pub metrics: [f32; 9],
    /// Body, inline code, code block. Other font families need host assets.
    pub monospace: [bool; 3],
    /// Inline background, inline text, link, inline border.
    pub colors: [[f32; 4]; 4],
    /// Top, right, bottom, left.
    pub padding: [f32; 4],
    pub border_width: f32,
    /// Top-left, top-right, bottom-right, bottom-left.
    pub radii: [f32; 4],
}

fn numbers<const N: usize>(values: [f32; N]) -> V {
    V::List(
        values
            .into_iter()
            .map(|value| V::F64(value.into()))
            .collect(),
    )
}
fn read_numbers<const N: usize>(value: &V) -> Option<[f32; N]> {
    let V::List(values) = value else { return None };
    if values.len() != N {
        return None;
    }
    values
        .iter()
        .map(|value| match value {
            V::F64(value) if value.is_finite() && value.abs() <= f64::from(f32::MAX) => {
                Some(*value as f32)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?
        .try_into()
        .ok()
}

impl MarkdownDocument {
    pub fn into_surface_value(self) -> V {
        V::Record {
            name: "ice.markdown".into(),
            fields: vec![
                ("source".into(), V::Str(self.source)),
                ("metrics".into(), numbers(self.metrics)),
                (
                    "monospace".into(),
                    V::List(self.monospace.into_iter().map(V::Bool).collect()),
                ),
                (
                    "colors".into(),
                    V::List(self.colors.into_iter().map(numbers).collect()),
                ),
                ("padding".into(), numbers(self.padding)),
                ("border_width".into(), V::F64(self.border_width.into())),
                ("radii".into(), numbers(self.radii)),
                (
                    "palette".into(),
                    V::List(self.palette.into_iter().map(numbers).collect()),
                ),
            ],
        }
    }

    /// Reject malformed records and bound geometry before native layout.
    pub fn from_surface_value(value: &V) -> Option<Self> {
        let V::Record { name, fields } = value else {
            return None;
        };
        let names = [
            "source",
            "metrics",
            "monospace",
            "colors",
            "padding",
            "border_width",
            "radii",
            "palette",
        ];
        if name != "ice.markdown"
            || fields.len() != names.len()
            || !fields
                .iter()
                .zip(names)
                .all(|((found, _), expected)| found == expected)
        {
            return None;
        }
        let V::Str(source) = &fields[0].1 else {
            return None;
        };
        let mut metrics: [f32; 9] = read_numbers(&fields[1].1)?;
        for size in &mut metrics[..8] {
            *size = size.clamp(f32::EPSILON, MAX_TEXT_PIXELS);
        }
        metrics[8] = metrics[8].clamp(0.0, MAX_TEXT_PIXELS);
        let V::List(fonts) = &fields[2].1 else {
            return None;
        };
        if fonts.len() != 3 {
            return None;
        }
        let monospace: [bool; 3] = fonts
            .iter()
            .map(|font| match font {
                V::Bool(value) => Some(*value),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?
            .try_into()
            .ok()?;
        let V::List(colors) = &fields[3].1 else {
            return None;
        };
        if colors.len() != 4 {
            return None;
        }
        let mut colors: [[f32; 4]; 4] = colors
            .iter()
            .map(read_numbers)
            .collect::<Option<Vec<_>>>()?
            .try_into()
            .ok()?;
        for color in &mut colors {
            for component in color {
                *component = component.clamp(0.0, 1.0);
            }
        }
        let mut padding = read_numbers(&fields[4].1)?;
        let V::F64(width) = fields[5].1 else {
            return None;
        };
        if !width.is_finite() {
            return None;
        }
        let mut radii = read_numbers(&fields[6].1)?;
        for value in padding.iter_mut().chain(radii.iter_mut()) {
            *value = value.clamp(0.0, MAX_TEXT_PIXELS);
        }
        let V::List(palette) = &fields[7].1 else {
            return None;
        };
        if palette.len() != 6 {
            return None;
        }
        let mut palette: [[f32; 4]; 6] = palette
            .iter()
            .map(read_numbers)
            .collect::<Option<Vec<_>>>()?
            .try_into()
            .ok()?;
        for color in &mut palette {
            for component in color {
                *component = component.clamp(0.0, 1.0);
            }
        }
        let mut source = source.clone();
        crate::truncate_string(&mut source);
        Some(Self {
            source,
            palette,
            metrics,
            monospace,
            colors,
            padding,
            border_width: width.clamp(0.0, f64::from(MAX_TEXT_PIXELS)) as f32,
            radii,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> MarkdownDocument {
        MarkdownDocument {
            source: "[Open](duck://docs)".into(),
            palette: [[0.5; 4]; 6],
            metrics: [18.0; 9],
            monospace: [false, true, true],
            colors: [[0.5; 4]; 4],
            padding: [2.0; 4],
            border_width: 1.0,
            radii: [3.0; 4],
        }
    }

    #[test]
    fn markdown_records_reject_malformed_shapes_and_nonfinite_settings() {
        let expected = document();
        let value = expected.clone().into_surface_value();
        assert_eq!(MarkdownDocument::from_surface_value(&value), Some(expected));
        let V::Record { name, fields } = value else {
            unreachable!()
        };
        let mut invalid = vec![V::Record {
            name: "other".into(),
            fields: fields.clone(),
        }];
        let mut swapped = fields.clone();
        swapped.swap(0, 1);
        invalid.push(V::Record {
            name: name.clone(),
            fields: swapped,
        });
        let mut missing = fields.clone();
        missing.pop();
        invalid.push(V::Record {
            name: name.clone(),
            fields: missing,
        });
        for (index, replacement) in [
            (0, V::Bool(false)),
            (1, V::List(vec![V::F64(18.0); 8])),
            (1, V::List(vec![V::F64(f64::NAN); 9])),
            (1, V::List(vec![V::F64(f64::MAX); 9])),
            (2, V::List(vec![V::Bool(false); 4])),
            (2, V::List(vec![V::Str("false".into()); 3])),
            (3, V::List(vec![numbers([0.0; 4]); 3])),
            (4, V::List(vec![V::F64(f64::INFINITY); 4])),
            (5, V::F64(f64::NEG_INFINITY)),
            (6, V::List(vec![V::F64(f64::NAN); 4])),
            (7, V::List(vec![numbers([0.0; 4]); 5])),
        ] {
            let mut changed = fields.clone();
            changed[index].1 = replacement;
            invalid.push(V::Record {
                name: name.clone(),
                fields: changed,
            });
        }
        for value in invalid {
            assert!(
                MarkdownDocument::from_surface_value(&value).is_none(),
                "malformed markdown must be rejected: {value:?}"
            );
        }
    }

    #[test]
    fn markdown_records_bound_text_geometry_and_colors() {
        let mut value = document();
        value.source = "한".repeat(crate::MAX_STRING_BYTES);
        value.metrics = [-1.0, 10000.0, 18.0, 18.0, 18.0, 18.0, 18.0, 18.0, -2.0];
        value.padding = [-10.0, 10000.0, 2.0, 3.0];
        value.radii = value.padding;
        value.border_width = 10000.0;
        value.colors[0] = [-1.0, 2.0, 0.5, 1.0];
        value.palette[1] = value.colors[0];
        let bounded = MarkdownDocument::from_surface_value(&value.into_surface_value()).unwrap();
        assert!(
            bounded.source.len() <= crate::MAX_STRING_BYTES,
            "markdown source must respect the UTF-8 byte limit"
        );
        assert!(bounded.source.ends_with('한'));
        assert_eq!(bounded.metrics[0], f32::EPSILON);
        assert_eq!(bounded.metrics[1], MAX_TEXT_PIXELS);
        assert_eq!(bounded.metrics[8], 0.0);
        assert_eq!(bounded.padding, [0.0, MAX_TEXT_PIXELS, 2.0, 3.0]);
        assert_eq!(bounded.radii, bounded.padding);
        assert_eq!(bounded.border_width, MAX_TEXT_PIXELS);
        assert_eq!(bounded.colors[0], [0.0, 1.0, 0.5, 1.0]);
        assert_eq!(bounded.palette[1], bounded.colors[0]);
    }
}
