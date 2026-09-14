//! The huddle's picture codec: one captured BGRA frame → intra-only JPEG
//! bytes for the wire, one peer's JPEG → BGRA pixels for the tile, and the
//! box-halving both sides use to hold a frame inside a pixel budget.
//!
//! THIS LIVES HERE, NOT IN THE APP, FOR THE DEV PROFILE. `jpeg_encoder::Encoder`
//! and `zune_jpeg::JpegDecoder` are generic over their writer and reader, so
//! their entry points and everything inlined into them are instantiated in the
//! crate that CALLS them — and the app crate is pinned at `opt-level = 0` in
//! dev, where a VGA encode cost ~25 ms, a decode ~44 ms and even the channel
//! swap ~24 ms (release: 3, 1.4 and 0.9). A per-package `opt-level` on the
//! codec crates alone left that copy unoptimized. Non-generic entry points in
//! this crate, which the dev profile optimizes (with the codec crates for
//! their own internals), bring a dev VGA frame to ~4 ms encode, ~4 ms decode
//! and ~0.4 ms swap — the capture loop keeps a camera's pace under `make dev`.
//!
//! BGRA END TO END: the renderer's `RenderImage` wants BGRA, so the decoder
//! produces it, the encoder reads it, and the only swap left is the camera's
//! RGBA once per frame ([`rgba_to_bgra_in_place`]).

/// The fixed v1 encode quality. 640×480 at this quality runs 30–60 KB, well
/// under the mesh's [`crate::video::MAX_FRAME_BYTES`].
pub const JPEG_QUALITY: u8 = 60;

/// One decoded picture: BGRA, `width * height * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// One 2×2 box-average pass over interleaved 4-channel pixels; odd edges
/// clamp their second sample.
pub fn halve(pixels: &[u8], width: u32, height: u32) -> Picture {
    let (out_w, out_h) = ((width / 2).max(1), (height / 2).max(1));
    let mut out = Vec::with_capacity(out_w as usize * out_h as usize * 4);
    for y in 0..out_h {
        let (y0, y1) = ((y * 2).min(height - 1), (y * 2 + 1).min(height - 1));
        for x in 0..out_w {
            let (x0, x1) = ((x * 2).min(width - 1), (x * 2 + 1).min(width - 1));
            for channel in 0..4 {
                let sample =
                    |sx: u32, sy: u32| u16::from(pixels[(sy * width + sx) as usize * 4 + channel]);
                let sum = sample(x0, y0) + sample(x1, y0) + sample(x0, y1) + sample(x1, y1);
                out.push((sum / 4) as u8);
            }
        }
    }
    Picture {
        pixels: out,
        width: out_w,
        height: out_h,
    }
}

/// Halve until `width * height` fits `budget` pixels.
pub fn shrink_to_budget(pixels: Vec<u8>, width: u32, height: u32, budget: u32) -> Picture {
    let mut picture = Picture {
        pixels,
        width,
        height,
    };
    while picture.width * picture.height > budget {
        picture = halve(&picture.pixels, picture.width, picture.height);
    }
    picture
}

/// The camera's RGBA becomes the renderer's BGRA in place.
pub fn rgba_to_bgra_in_place(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Encode one BGRA frame to the wire's opaque bytes (alpha is dropped).
///
/// A frame over the mesh cap cannot be sent as it is — and a source that
/// overruns once overruns every frame, which is a stream that stops dead
/// with no error anywhere. A busy screen is exactly that source, so its
/// resolution is traded rather than its liveness: half the size, one more
/// try, and `None` only when even that overruns.
pub fn encode_bgra(pixels: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let cap = crate::video::MAX_FRAME_BYTES;
    let out = encode_once(pixels, width, height)?;
    if out.len() <= cap {
        return Some(out);
    }
    let small = halve(pixels, width, height);
    let out = encode_once(&small.pixels, small.width, small.height)?;
    (out.len() <= cap).then_some(out)
}

fn encode_once(pixels: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (width, height) = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder
        .encode(pixels, width, height, jpeg_encoder::ColorType::Bgra)
        .ok()?;
    Some(out)
}

/// Decode a peer's JPEG to BGRA: a frame declaring more than `refuse_over`
/// pixels is refused BEFORE any output is allocated, and one inside it is
/// halved onto `shrink_to`.
///
/// The gate reads the SOF through `decode_headers`, which stops at the SOS
/// marker and never sizes a buffer off width×height. zune-core's own
/// `max_width`/`max_height` are per-axis and checked with a strict `>`, so a
/// 16384×16384 attack sailed through them and allocated a 1 GiB output; the
/// budgets here are AREAS, and area is what is checked.
pub fn decode_bgra(data: &[u8], refuse_over: u32, shrink_to: u32) -> Option<Picture> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let mut decoder = zune_jpeg::JpegDecoder::new(data);
    decoder.set_options(DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA));
    decoder.decode_headers().ok()?;
    let (width, height) = decoder.dimensions()?;
    let (width, height) = (width as u32, height as u32);
    if u64::from(width) * u64::from(height) > u64::from(refuse_over) {
        return None;
    }
    let pixels = decoder.decode().ok()?;
    Some(shrink_to_budget(pixels, width, height, shrink_to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_at_its_own_size() {
        let (width, height) = (64u32, 48u32);
        let bgra: Vec<u8> = (0..width * height)
            .flat_map(|i| [(i % 251) as u8, (i % 83) as u8, (i % 199) as u8, 0xff])
            .collect();
        let encoded = encode_bgra(&bgra, width, height).expect("encode");
        assert!(encoded.len() < crate::video::MAX_FRAME_BYTES);
        let picture = decode_bgra(&encoded, 1 << 20, 1 << 20).expect("decode");
        assert_eq!((picture.width, picture.height), (64, 48));
        assert_eq!(picture.pixels.len(), 64 * 48 * 4);
        // the encoder reads BGRA: a pure blue frame comes back blue, not red
        let blue = [0xff, 0x00, 0x00, 0xff].repeat(16 * 16);
        let encoded = encode_bgra(&blue, 16, 16).expect("encode");
        let picture = decode_bgra(&encoded, 1 << 20, 1 << 20).expect("decode");
        let [b, g, r, _] = picture.pixels[..4] else {
            unreachable!()
        };
        assert!(b > 0xf0 && g < 0x10 && r < 0x10, "bgra kept: {b} {g} {r}");
    }

    /// Bytes for a JPEG whose SOF declares `width`×`height` and nothing else —
    /// enough to probe the size gate without a valid image.
    fn crafted_sof_bytes(width: u16, height: u16) -> Vec<u8> {
        let [h_hi, h_lo] = height.to_be_bytes();
        let [w_hi, w_lo] = width.to_be_bytes();
        vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, // SOF0
            0x00, 0x0B, // length = 8 + 3*1 components
            0x08, // precision
            h_hi, h_lo, // height
            w_hi, w_lo, // width
            0x01, // one component
            0x01, 0x11, 0x00, // id=1, h/v sample=1/1, quant table 0
            0xFF, 0xDA, // SOS
            0x00, 0x08, // length = 6 + 2*1 components
            0x01, // one component in scan
            0x01, 0x00, // component id=1, DC/AC huffman table 0/0
            0x00, 0x3F, 0x00, // spectral start/end, approximation
        ]
    }

    /// THE ATTACK IN #1791: a SOF on zune-core's per-axis default passed its
    /// strict `>` guard and allocated a 1 GiB output. The area gate refuses it
    /// before `decode()` runs at all.
    #[test]
    fn an_oversized_declared_frame_is_refused_before_any_allocation() {
        let budget = 512 * 1024 - 1;
        assert!(decode_bgra(&crafted_sof_bytes(16384, 16384), budget, budget).is_none());
    }

    #[test]
    fn halving_boxes_pixels_and_respects_the_budget() {
        let quad = [
            0, 0, 0, 0, 40, 40, 40, 40, 80, 80, 80, 80, 120, 120, 120, 120,
        ];
        let half = halve(&quad, 2, 2);
        assert_eq!((half.width, half.height), (1, 1));
        assert_eq!(half.pixels, vec![60, 60, 60, 60]);
        // 720p lands at 640×360 under a 512 Ki pixel budget
        let picture = shrink_to_budget(vec![7; 1280 * 720 * 4], 1280, 720, 512 * 1024 - 1);
        assert_eq!((picture.width, picture.height), (640, 360));
        assert!(picture.pixels.iter().all(|&byte| byte == 7));
        // a frame already inside the budget passes through untouched
        let picture = shrink_to_budget(vec![9; 64 * 48 * 4], 64, 48, 640 * 480);
        assert_eq!(
            (picture.width, picture.height, picture.pixels.len()),
            (64, 48, 64 * 48 * 4)
        );
    }

    #[test]
    fn the_camera_swap_is_in_place() {
        let mut pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        rgba_to_bgra_in_place(&mut pixels);
        assert_eq!(pixels, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
