//! Proves the vendored hayro-interpret patch (vendor/hayro-interpret/src/x_object.rs):
//! decoded raster images are downsampled toward the renderer's
//! `target_dimension` hint before they're returned to the caller (and, in
//! the real app, cached/composited) -- for EVERY filter, not just DCTDecode
//! (which the vendored hayro-syntax patch already handles at the decode
//! layer). These fixtures use FlateDecode specifically, since it's the most
//! common filter for PDF raster images and, unlike DCTDecode, has no
//! filter-level scaled-decode path of its own.
//!
//! Modeled on tests/dct_downsample.rs: a discriminating test that fails
//! without the patch, an unscaled control pinning the original behavior,
//! and two end-to-end `hayro::render` composition checks (plain image, and
//! an image + independently-sized `/SMask`).

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, ImageData, InterpreterCache,
    InterpreterSettings, LumaData, Paint, PathDrawMode, SoftMask, TransformExt, interpret_page,
};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render};
use kurbo::{Affine, BezPath, Rect};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use std::io::Write;

const NATIVE_W: u32 = 1600;
const NATIVE_H: u32 = 1200;
const COLOR: [u8; 3] = [180, 90, 40];

// ---------------------------------------------------------------------
// PDF / pixel-buffer builders
// ---------------------------------------------------------------------

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .expect("writing to an in-memory zlib encoder must succeed");
    encoder
        .finish()
        .expect("finishing an in-memory zlib stream must succeed")
}

fn solid_rgb_bytes(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let mut data = Vec::with_capacity(width as usize * height as usize * 3);
    for _ in 0..(width as usize * height as usize) {
        data.extend_from_slice(&color);
    }
    data
}

fn solid_gray_bytes(width: u32, height: u32, value: u8) -> Vec<u8> {
    vec![value; width as usize * height as usize]
}

/// Build an Image XObject dict+stream body (everything between the object
/// number's `obj`/`endobj`, exclusive) for a FlateDecode raster, optionally
/// referencing an `/SMask` by indirect object number.
fn image_object_body(
    raw: &[u8],
    width: u32,
    height: u32,
    color_space: &str,
    smask_obj: Option<usize>,
) -> Vec<u8> {
    let compressed = zlib_compress(raw);
    let smask_entry = smask_obj
        .map(|n| format!(" /SMask {n} 0 R"))
        .unwrap_or_default();
    let header = format!(
        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} \
         /ColorSpace {color_space} /BitsPerComponent 8 /Filter /FlateDecode{smask_entry} \
         /Length {} >>\nstream\n",
        compressed.len()
    );
    [header.into_bytes(), compressed, b"\nendstream".to_vec()].concat()
}

/// Build a minimal single-page PDF (hand-written xref, no compression on the
/// page/content objects themselves) that draws image object 5 as a full-page
/// `/Im0` XObject. Modeled on tests/dct_downsample.rs's `build_pdf`, extended
/// with an optional trailing SMask object (6).
fn build_pdf(image_body: Vec<u8>, smask_body: Option<Vec<u8>>) -> Vec<u8> {
    let content = b"q 612 0 0 792 0 0 cm /Im0 Do Q".to_vec();

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
    objects.push(image_body);
    if let Some(smask_body) = smask_body {
        objects.push(smask_body);
    }

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

fn build_pdf_rgb(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let body = image_object_body(
        &solid_rgb_bytes(width, height, color),
        width,
        height,
        "/DeviceRGB",
        None,
    );
    build_pdf(body, None)
}

fn build_pdf_rgb_with_smask(
    color_w: u32,
    color_h: u32,
    color: [u8; 3],
    alpha_w: u32,
    alpha_h: u32,
    alpha_value: u8,
) -> Vec<u8> {
    let color_body = image_object_body(
        &solid_rgb_bytes(color_w, color_h, color),
        color_w,
        color_h,
        "/DeviceRGB",
        Some(6),
    );
    let smask_body = image_object_body(
        &solid_gray_bytes(alpha_w, alpha_h, alpha_value),
        alpha_w,
        alpha_h,
        "/DeviceGray",
        None,
    );
    build_pdf(color_body, Some(smask_body))
}

// ---------------------------------------------------------------------
// A capturing Device that grabs the RasterImage's decoded ImageData/alpha
// -- i.e. the same seam the real renderer drives (hayro-0.7.1's
// `Renderer::draw_image`, `Image::Raster(r) => r.with_rgba(..., Some((target_width, target_height)))`)
// but with a caller-chosen target_dimension instead of one derived from the
// page transform.
// ---------------------------------------------------------------------

struct CaptureDevice {
    captured: Option<(ImageData, Option<LumaData>)>,
    target: Option<(u32, u32)>,
}

impl<'a> Device<'a> for CaptureDevice {
    fn set_soft_mask(&mut self, _mask: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _blend_mode: BlendMode) {}
    fn draw_path(
        &mut self,
        _path: &BezPath,
        _transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &PathDrawMode,
    ) {
    }
    fn push_clip_path(&mut self, _clip_path: &ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _opacity: f32,
        _mask: Option<SoftMask<'a>>,
        _blend_mode: BlendMode,
    ) {
    }
    fn draw_glyph(
        &mut self,
        _glyph: &Glyph<'a>,
        _transform: Affine,
        _glyph_transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &GlyphDrawMode,
    ) {
    }
    fn draw_image(&mut self, image: Image<'a, '_>, _transform: Affine) {
        if let Image::Raster(r) = image {
            let target = self.target;
            r.with_rgba(
                |img, alpha| {
                    self.captured = Some((img, alpha));
                },
                target,
            );
        }
    }
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Interpret `pdf`'s single page's content stream, capturing the ImageData
/// (and optional alpha) delivered to the one `/Im0 Do` raster image draw.
fn capture_raster(pdf: &Pdf, target_dimension: Option<(u32, u32)>) -> (ImageData, Option<LumaData>) {
    let page = &pdf.pages()[0];
    let initial_transform = page.initial_transform(true).to_kurbo();
    let (width, height) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        initial_transform,
        Rect::new(0.0, 0.0, width as f64, height as f64),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut device = CaptureDevice {
        captured: None,
        target: target_dimension,
    };

    interpret_page(page, &mut ctx, &mut device);

    device
        .captured
        .expect("the page's one image draw must reach CaptureDevice::draw_image")
}

// ---------------------------------------------------------------------
// Pixel helpers
// ---------------------------------------------------------------------

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

fn avg_gray(data: &[u8]) -> f64 {
    let n = data.len().max(1) as f64;
    data.iter().map(|&b| b as f64).sum::<f64>() / n
}

fn image_data_avg_rgb(image: &ImageData) -> [f64; 3] {
    match image {
        ImageData::Rgb(d) => avg_rgb(&d.data),
        ImageData::Luma(d) => {
            let g = avg_gray(&d.data);
            [g, g, g]
        }
    }
}

fn image_data_dims(image: &ImageData) -> (u32, u32) {
    (image.width(), image.height())
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

// ---------------------------------------------------------------------
// Test A -- DISCRIMINATING: without the patch, this fails -- the delivered
// ImageData comes back at the native 1600x1200 instead of being reduced
// toward the (100, 100) hint.
//
// MUTATION CHECK (2026-08-17): with `downsample_to_target` in
// vendor/hayro-interpret/src/x_object.rs neutered to return immediately
// (before touching data/width/height), this test failed exactly as
// expected:
//
//   thread 'flate_scaled_decode_honors_target_dimension' panicked at tests/interpret_downsample.rs:...:
//   expected a reduced decode close to the (100, 100) hint, got 1600x1200
//   (native was 1600x1200) -- the target_dimension downsample path did not run
//
// The neutering was reverted immediately after confirming the failure.
// ---------------------------------------------------------------------
#[test]
fn flate_scaled_decode_honors_target_dimension() {
    let pdf_bytes = build_pdf_rgb(NATIVE_W, NATIVE_H, COLOR);
    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse");

    let (image, alpha) = capture_raster(&pdf, Some((100, 100)));
    assert!(alpha.is_none(), "this fixture has no SMask/Mask");

    let (w, h) = image_data_dims(&image);
    assert!(
        w <= 800 && h <= 600,
        "expected a reduced decode close to the (100, 100) hint, got {w}x{h} \
         (native was {NATIVE_W}x{NATIVE_H}) -- the target_dimension downsample path did not run"
    );
    assert!(
        w < NATIVE_W && h < NATIVE_H,
        "reduced dims must be strictly smaller than native: got {w}x{h}"
    );

    let avg = image_data_avg_rgb(&image);
    assert_close(avg, COLOR, 8.0, "scaled decode");
}

// ---------------------------------------------------------------------
// Test B -- unscaled control: with no target_dimension hint, dims come back
// exactly native and the pixel-average check still holds. Pins the
// untouched decode path (and, on its own, the bit-exactness invariant: with
// target_dimension: None, decode_raster's downsample block never runs).
// ---------------------------------------------------------------------
#[test]
fn flate_unscaled_decode_is_exact() {
    let pdf_bytes = build_pdf_rgb(NATIVE_W, NATIVE_H, COLOR);
    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse");

    let (image, alpha) = capture_raster(&pdf, None);
    assert!(alpha.is_none(), "this fixture has no SMask/Mask");

    let (w, h) = image_data_dims(&image);
    assert_eq!(
        (w, h),
        (NATIVE_W, NATIVE_H),
        "unscaled decode must return the native dimensions exactly"
    );

    let avg = image_data_avg_rgb(&image);
    assert_close(avg, COLOR, 8.0, "unscaled decode");
}

// ---------------------------------------------------------------------
// Test C -- end-to-end: hayro::render on the mini-PDF at a small viewport
// scale must still composite the (much larger) source image down
// correctly, proving the reduced-dims ImageData flows through
// hayro-interpret's scale_factors path and the renderer without visible
// drift -- both at the center AND near the edges of the drawn area (a wrong
// scale_factors shows up as a shrunken image patch on the page, which a
// center-only check would miss).
// ---------------------------------------------------------------------
#[test]
fn flate_scaled_decode_composes_end_to_end() {
    let pdf_bytes = build_pdf_rgb(NATIVE_W, NATIVE_H, COLOR);
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
    assert!(w > 4 && h > 4, "rendered pixmap must be non-trivially sized");

    let data = pixmap.data_as_u8_slice();
    let pixel_at = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * w + x) * 4;
        [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
    };

    // The content stream fills the entire page (and thus the entire
    // pixmap) with the image, so its edges ARE the pixmap's edges; inset by
    // 2px to stay clear of edge anti-aliasing.
    for (label, (x, y)) in [
        ("center", (w / 2, h / 2)),
        ("near top-left edge", (2, 2)),
        ("near bottom-right edge", (w - 3, h - 3)),
    ] {
        let px = pixel_at(x, y);
        assert_eq!(px[3], 255, "{label} pixel must be fully opaque");
        for i in 0..3 {
            let diff = (px[i] as f64 - COLOR[i] as f64).abs();
            assert!(
                diff <= 16.0,
                "{label} pixel channel {i} = {} vs expected {} (diff {diff:.2} > tol 16)",
                px[i],
                COLOR[i]
            );
        }
    }
}

// ---------------------------------------------------------------------
// Test D -- alpha/SMask: the image has an /SMask with DIFFERENT dimensions
// than the color data (800x600 vs. 1600x1200). With a (100, 100) target,
// both must arrive independently reduced, and must still composite
// correctly end-to-end (semi-transparent color over the white background).
// ---------------------------------------------------------------------
const SMASK_W: u32 = 800;
const SMASK_H: u32 = 600;
const ALPHA_VALUE: u8 = 128;

#[test]
fn flate_smask_scales_and_composites() {
    let pdf_bytes =
        build_pdf_rgb_with_smask(NATIVE_W, NATIVE_H, COLOR, SMASK_W, SMASK_H, ALPHA_VALUE);
    let pdf = Pdf::new(pdf_bytes.clone()).expect("test PDF must parse");

    // -- decode-level: color and alpha both come back reduced, independently.
    let (image, alpha) = capture_raster(&pdf, Some((100, 100)));
    let alpha = alpha.expect("this fixture has an /SMask");

    let (cw, ch) = image_data_dims(&image);
    assert!(
        cw < NATIVE_W && ch < NATIVE_H,
        "color data must be reduced: got {cw}x{ch}, native was {NATIVE_W}x{NATIVE_H}"
    );
    assert!(
        alpha.width < SMASK_W && alpha.height < SMASK_H,
        "alpha data must be reduced: got {}x{}, native was {SMASK_W}x{SMASK_H}",
        alpha.width,
        alpha.height
    );

    let color_avg = image_data_avg_rgb(&image);
    assert_close(color_avg, COLOR, 8.0, "SMask fixture color data");

    let alpha_avg = avg_gray(&alpha.data);
    assert!(
        (alpha_avg - ALPHA_VALUE as f64).abs() <= 8.0,
        "alpha avg {alpha_avg:.2} vs expected {ALPHA_VALUE} (diff > tol 8)"
    );

    // -- end-to-end: renders as color blended over the white background at
    // the SMask's alpha.
    let pdf = Pdf::new(pdf_bytes).expect("test PDF must parse (second parse for render)");
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
    let data = pixmap.data_as_u8_slice();
    let (cx, cy) = (w / 2, h / 2);
    let idx = (cy * w + cx) * 4;
    let px = &data[idx..idx + 4];

    // Opaque WHITE background fills the whole canvas, so the final
    // composited alpha is fully opaque and the premultiplied byte equals
    // the un-premultiplied blended color (same reasoning as Test C).
    assert_eq!(px[3], 255, "center pixel must be fully opaque");

    let a = ALPHA_VALUE as f64 / 255.0;
    let expected = [
        COLOR[0] as f64 * a + 255.0 * (1.0 - a),
        COLOR[1] as f64 * a + 255.0 * (1.0 - a),
        COLOR[2] as f64 * a + 255.0 * (1.0 - a),
    ];

    for i in 0..3 {
        let diff = (px[i] as f64 - expected[i]).abs();
        assert!(
            diff <= 10.0,
            "center pixel channel {i} = {} vs expected blended {:.2} (diff {diff:.2} > tol 10)",
            px[i],
            expected[i]
        );
    }
}
