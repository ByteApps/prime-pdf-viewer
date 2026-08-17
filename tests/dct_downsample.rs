//! Proves the vendored hayro-syntax DCTDecode patch (vendor/hayro-syntax/src/filter/dct.rs):
//! a scaled decode via `jpeg-decoder`'s IDCT downscaling when
//! `ImageDecodeParams::target_dimension` hints that the renderer will draw
//! the image much smaller than its native size.
//!
//! `hayro-syntax` is not a workspace member (it's pulled in only via
//! `[patch.crates-io]`), so it can't be tested with `cargo test -p
//! hayro-syntax`. Instead this test drives it through the app's own
//! dependency graph: `hayro` re-exports `hayro_interpret`, which re-exports
//! `hayro_syntax` in full.

use hayro::hayro_interpret::hayro_syntax::object::Name;
use hayro::hayro_interpret::hayro_syntax::object::stream::ImageDecodeParams;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};

use image::{Rgb, RgbImage};

/// Encode a solid-color JPEG in memory. High quality keeps the DCT/IDCT
/// round trip close to lossless for a flat color, which is what makes the
/// pixel-average assertions below meaningful.
fn make_solid_jpeg(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let img = RgbImage::from_pixel(width, height, Rgb(color));
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92);
    encoder
        .encode_image(&img)
        .expect("encoding the test JPEG must succeed");
    buf
}

/// Build a minimal single-page PDF (hand-written xref, no compression) that
/// draws `jpeg` as a full-page Image XObject named `/Im0`. Modeled on
/// `ui-automation/fixtures/gen-sample-pdf.py`.
fn build_pdf(jpeg: &[u8], img_w: u32, img_h: u32) -> Vec<u8> {
    let content = b"q 612 0 0 792 0 0 cm /Im0 Do Q".to_vec();

    // Object bodies, 1-indexed by position in this vec.
    let mut objects: Vec<Vec<u8>> = Vec::new();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    objects.push(
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
    );
    objects.push(
        [
            format!("<< /Length {} >>\nstream\n", content.len()).into_bytes(),
            content.clone(),
            b"\nendstream".to_vec(),
        ]
        .concat(),
    );
    objects.push(
        [
            format!(
                "<< /Type /XObject /Subtype /Image /Width {img_w} /Height {img_h} \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode \
                 /Length {} >>\nstream\n",
                jpeg.len()
            )
            .into_bytes(),
            jpeg.to_vec(),
            b"\nendstream".to_vec(),
        ]
        .concat(),
    );

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (n, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", n + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }

    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );

    pdf
}

/// Average per-channel value over an RGB8 byte buffer (3 bytes/pixel).
fn avg_rgb(data: &[u8]) -> [f64; 3] {
    let n = (data.len() / 3).max(1) as f64;
    let mut sum = [0.0f64; 3];
    for chunk in data.chunks_exact(3) {
        sum[0] += chunk[0] as f64;
        sum[1] += chunk[1] as f64;
        sum[2] += chunk[2] as f64;
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

fn assert_close(avg: [f64; 3], expected: [u8; 3], tol: f64, label: &str) {
    for i in 0..3 {
        let diff = (avg[i] - expected[i] as f64).abs();
        assert!(
            diff <= tol,
            "{label}: channel {i} avg {:.2} vs expected {} (diff {diff:.2} > tol {tol})",
            avg[i],
            expected[i]
        );
    }
}

const COLOR: [u8; 3] = [180, 90, 40];
const NATIVE_W: u32 = 1600;
const NATIVE_H: u32 = 1200;

/// DISCRIMINATING: without the patch, this fails — the returned image_data
/// comes back at the native 1600x1200 instead of being reduced toward the
/// (100, 100) hint.
#[test]
fn dct_scaled_decode_honors_target_dimension() {
    let jpeg = make_solid_jpeg(NATIVE_W, NATIVE_H, COLOR);
    let pdf_bytes = build_pdf(&jpeg, NATIVE_W, NATIVE_H);

    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse");
    let page = &pdf.pages()[0];
    let name = Name::new(b"Im0").expect("valid PDF name");
    let stream = page
        .resources()
        .get_x_object(&name)
        .expect("Im0 XObject must be present");

    let params = ImageDecodeParams {
        is_indexed: false,
        bpc: Some(8),
        num_components: Some(3),
        target_dimension: Some((100, 100)),
        width: NATIVE_W,
        height: NATIVE_H,
    };
    let result = stream
        .decoded_image(&params)
        .expect("DCTDecode must succeed");
    let image_data = result
        .image_data
        .expect("DCTDecode must report image_data");

    assert!(
        image_data.width <= 800 && image_data.height <= 600,
        "expected a reduced decode close to the (100, 100) hint, got {}x{} \
         (native was {NATIVE_W}x{NATIVE_H}) -- the target_dimension scaled \
         path did not run",
        image_data.width,
        image_data.height
    );
    assert!(
        image_data.width < NATIVE_W && image_data.height < NATIVE_H,
        "reduced dims must be strictly smaller than native: got {}x{}",
        image_data.width,
        image_data.height
    );

    let avg = avg_rgb(&result.data);
    assert_close(avg, COLOR, 12.0, "scaled decode");
}

/// Control: with no target_dimension hint, dims come back exactly native and
/// the pixel-average check still holds. Pins the untouched zune-jpeg path.
#[test]
fn dct_unscaled_decode_is_exact() {
    let jpeg = make_solid_jpeg(NATIVE_W, NATIVE_H, COLOR);
    let pdf_bytes = build_pdf(&jpeg, NATIVE_W, NATIVE_H);

    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse");
    let page = &pdf.pages()[0];
    let name = Name::new(b"Im0").expect("valid PDF name");
    let stream = page
        .resources()
        .get_x_object(&name)
        .expect("Im0 XObject must be present");

    let params = ImageDecodeParams {
        is_indexed: false,
        bpc: Some(8),
        num_components: Some(3),
        target_dimension: None,
        width: NATIVE_W,
        height: NATIVE_H,
    };
    let result = stream
        .decoded_image(&params)
        .expect("DCTDecode must succeed");
    let image_data = result
        .image_data
        .expect("DCTDecode must report image_data");

    assert_eq!(
        (image_data.width, image_data.height),
        (NATIVE_W, NATIVE_H),
        "unscaled decode must return the native dimensions exactly"
    );

    let avg = avg_rgb(&result.data);
    assert_close(avg, COLOR, 12.0, "unscaled decode");
}

/// End-to-end: hayro::render on the mini-PDF at a small viewport scale must
/// still composite the (much larger) source image down correctly, proving
/// the reduced-dims FilterResult flows through hayro-interpret's
/// scale_factors path and the renderer without visible drift.
#[test]
fn dct_scaled_decode_composes_end_to_end() {
    let jpeg = make_solid_jpeg(NATIVE_W, NATIVE_H, COLOR);
    let pdf_bytes = build_pdf(&jpeg, NATIVE_W, NATIVE_H);

    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse");
    let page = &pdf.pages()[0];

    let cache = RenderCache::new();
    let pixmap = render(
        page,
        &cache,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: 0.15,
            y_scale: 0.15,
            bg_color: WHITE,
            ..Default::default()
        },
    );

    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    assert!(w > 0 && h > 0, "rendered pixmap must be non-empty");

    let data = pixmap.data_as_u8_slice();
    let (cx, cy) = (w / 2, h / 2);
    let idx = (cy * w + cx) * 4;
    let px = &data[idx..idx + 4];

    // Premultiplied RGBA8 over an opaque image on a WHITE background: alpha
    // should be fully opaque and the un-premultiplied color should match.
    assert_eq!(px[3], 255, "center pixel must be fully opaque");
    for i in 0..3 {
        let diff = (px[i] as f64 - COLOR[i] as f64).abs();
        assert!(
            diff <= 16.0,
            "center pixel channel {i} = {} vs expected {} (diff {diff:.2} > tol 16)",
            px[i],
            COLOR[i]
        );
    }
}
