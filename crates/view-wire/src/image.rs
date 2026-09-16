use serde::{Deserialize, Serialize};

/// Copied raster data or an opaque host image resource. Native allocations never cross.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageData {
    Encoded(#[serde(deserialize_with = "decode_bytes")] Vec<u8>),
    Rgba {
        width: u32,
        height: u32,
        #[serde(deserialize_with = "decode_bytes")]
        pixels: Vec<u8>,
    },
    /// A host-issued image key, resolved at paint time rather than cached pixels.
    /// This is neither a filesystem path nor a URL.
    Resource(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFilter {
    #[default]
    Linear,
    Nearest,
}

impl ImageData {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Resource(key) => key.len(),
            Self::Encoded(bytes) => bytes.len(),
            Self::Rgba { pixels, .. } => pixels.len(),
        }
    }

    pub fn valid_rgba(&self) -> bool {
        match self {
            Self::Resource(key) => !key.is_empty() && !key.contains('\0'),
            Self::Encoded(_) => true,
            Self::Rgba {
                width,
                height,
                pixels,
            } => {
                *width != 0
                    && *height != 0
                    && u64::from(*width)
                        .checked_mul(u64::from(*height))
                        .and_then(|count| count.checked_mul(4))
                        == Some(pixels.len() as u64)
            }
        }
    }

    pub(crate) fn sanitize(data: &mut Option<Self>, budgets: &mut crate::Budgets) {
        if let Some(value) = data {
            if !value.valid_rgba() || value.byte_len() > budgets.pictures {
                *data = None;
            } else {
                budgets.pictures -= value.byte_len();
            }
        }
    }
}

// A wire frame is capped at 8 MiB by hosts. Reject a collection header before
// allocating, even when callers decode ImageData directly. The smaller shared
// picture allowance is applied by frame sanitization, without truncating data.
fn decode_bytes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct Bytes;
    impl<'de> serde::de::Visitor<'de> for Bytes {
        type Value = Vec<u8>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("bounded raster bytes")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            const LIMIT: usize = 8 << 20;
            if seq.size_hint().is_some_and(|size| size > LIMIT) {
                return Err(serde::de::Error::custom("raster byte limit exceeded"));
            }
            let mut bytes = Vec::new();
            while let Some(byte) = seq.next_element()? {
                if bytes.len() == LIMIT {
                    return Err(serde::de::Error::custom("raster byte limit exceeded"));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
    deserializer.deserialize_seq(Bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_images_roundtrip_and_refuse_malicious_collection_headers() {
        for image in [
            ImageData::Encoded(vec![0, 255]),
            ImageData::Resource("image:7".into()),
            ImageData::Rgba {
                width: 1,
                height: 1,
                pixels: vec![255; 4],
            },
        ] {
            assert_eq!(
                crate::decode::<ImageData>(&crate::encode(&image)).unwrap(),
                image
            );
        }
        let mut malicious = 0u32.to_le_bytes().to_vec();
        malicious.extend_from_slice(&u64::MAX.to_le_bytes());
        let error = crate::decode::<ImageData>(&malicious)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("raster byte limit"),
            "reject the header before reading elements: {error}"
        );
    }
    #[test]
    fn invalid_rgba_is_dropped_without_spending_valid_picture_budget() {
        let mut budget = crate::Budgets::frame();
        budget.pictures = 4;
        let mut invalid = Some(ImageData::Rgba {
            width: u32::MAX,
            height: u32::MAX,
            pixels: vec![0; 4],
        });
        ImageData::sanitize(&mut invalid, &mut budget);
        assert_eq!(invalid, None);
        assert_eq!(budget.pictures, 4);
        let mut valid = Some(ImageData::Rgba {
            width: 1,
            height: 1,
            pixels: vec![255; 4],
        });
        ImageData::sanitize(&mut valid, &mut budget);
        assert!(valid.is_some());
        assert_eq!(budget.pictures, 0);
        let mut excess = Some(ImageData::Encoded(vec![1]));
        ImageData::sanitize(&mut excess, &mut budget);
        assert_eq!(excess, None);
    }
}

/// Copied native viewer settings; absent values retain the host's defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewerOptions {
    pub padding: Option<f32>,
    pub scale_bounds: Option<(f32, f32)>,
    pub scale_step: Option<f32>,
}
impl ViewerOptions {
    pub(crate) fn sanitize(&mut self) {
        super::bound_optional(&mut self.padding);
        if let Some((min, max)) = &mut self.scale_bounds {
            (*min, *max) = viewer_scale_bounds(f64::from(*min), f64::from(*max));
        }
        if let Some(step) = &mut self.scale_step {
            *step = viewer_scale_bounds(f64::from(*step), f64::from(*step)).0;
        }
    }
}

/// Converts viewer scale bounds to a finite, positive, ordered `f32` range.
pub fn viewer_scale_bounds(min: f64, max: f64) -> (f32, f32) {
    let positive = |value: f64| {
        let value = value as f32;
        if value.is_nan() {
            f32::EPSILON
        } else {
            value.clamp(f32::EPSILON, f32::MAX)
        }
    };
    let min = positive(min);
    let max = positive(max);
    (min.min(max), min.max(max))
}

#[cfg(test)]
mod viewer_tests {
    use super::*;
    #[test]
    fn viewer_options_keep_native_positive_ordered_finite_bounds() {
        let mut options = ViewerOptions {
            padding: Some(f32::NAN),
            scale_bounds: Some((f32::INFINITY, f32::NAN)),
            scale_step: Some(f32::NEG_INFINITY),
        };
        options.sanitize();
        assert_eq!(options.padding, Some(0.0));
        assert_eq!(options.scale_bounds, Some((f32::EPSILON, f32::MAX)));
        assert_eq!(options.scale_step, Some(f32::EPSILON));
        options.scale_bounds = Some((4.0, 0.5));
        options.sanitize();
        assert_eq!(options.scale_bounds, Some((0.5, 4.0)));
    }
}
