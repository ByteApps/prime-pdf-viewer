//! Measure what opening a PDF actually costs in memory.
//!
//! The device crashed on an ~11 MB PDF, and "how big is too big" cannot be
//! read off any manifest -- KeyOS declares no per-app heap budget the SDK
//! exposes. So measure the part we CAN measure: how peak RSS scales with the
//! input, for the same `hayro` version the app ships.
//!
//! Peak RSS is the honest number here (not the file size), because a load+
//! render holds, at once: the whole file, hayro's parsed objects and any
//! decompressed images for the page, hayro's pixmap, AND the app's copy of
//! that pixmap in a `SharedPixelBuffer`.
//!
//!   cargo run --example mem_probe -- <file.pdf> [more.pdf ...]
//!
//! Run on the HOST. It measures hayro's appetite, which is platform-neutral;
//! the device's ceiling is a separate number that only hardware can tell us.

use std::sync::Arc;

use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::{render, RenderCache, RenderSettings};

const PAGE_WIDTH: f32 = 440.0; // must match src/main.rs
const MAX_PAGE_HEIGHT: f32 = 4096.0;

// Mirror of the cost-model constants in src/main.rs -- keep them in step.
const PAGE_RENDER_FIXED_BYTES: u64 = 8 * 1024 * 1024;
const PAGE_IMAGE_FACTOR: u64 = 2;
const MAX_PAGE_RENDER_BYTES: u64 = 32 * 1024 * 1024;

/// Peak resident set size in bytes, as the kernel measured it for this process.
/// `ru_maxrss` is a high-water mark, so it never under-reports a transient
/// spike the way sampling would.
fn peak_rss() -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct RUsage {
        ru_utime: [i64; 2],
        ru_stime: [i64; 2],
        ru_maxrss: i64,
        rest: [i64; 14],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }
    let mut u = RUsage::default();
    // RUSAGE_SELF == 0. macOS reports ru_maxrss in BYTES (Linux: kilobytes).
    unsafe { getrusage(0, &mut u) };
    if cfg!(target_os = "macos") { u.ru_maxrss as u64 } else { u.ru_maxrss as u64 * 1024 }
}

fn mb(bytes: u64) -> f64 { bytes as f64 / (1024.0 * 1024.0) }

/// CURRENT resident size, via ps. Peak RSS only ever rises, so it cannot tell
/// "this page cost 8 MB and freed it" from "this page kept 8 MB" — and that
/// distinction decides whether the fix is a cap or a cache reset.
fn current_rss() -> u64 {
    let pid = std::process::id();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Mirror of `image_cost_bytes` in src/main.rs -- keep them in step.
fn image_cost_bytes(native_w: u64, native_h: u64, bound_w: u64, bound_h: u64) -> u64 {
    let native_area = native_w.saturating_mul(native_h);
    let bound_area = bound_w.saturating_mul(bound_h);
    4u64.saturating_mul(native_area.min(bound_area))
}

/// Mirror of `page_image_bytes` in src/main.rs -- keep them in step. Scale-
/// aware since the vendored downsample patches make render scale a real
/// lever on decode cost: an image can't cost more than ~2x its drawn size,
/// bounded here by the whole page's pixmap at `scale` (see src/main.rs's
/// doc comment for why the bound is page-shaped, not per-image).
fn page_image_bytes(page: &hayro::hayro_interpret::hayro_syntax::page::Page<'_>, scale: f32) -> u64 {
    use hayro::hayro_interpret::hayro_syntax::object::{Dict, Name, Stream};
    fn walk(xobjects: &Dict<'_>, depth: u32, bound_w: u64, bound_h: u64, total: &mut u64) {
        if depth > 4 {
            return;
        }
        for key in xobjects.keys() {
            let Some(stream) = xobjects.get::<Stream<'_>>(key.as_ref()) else { continue };
            let dict = stream.dict();
            let subtype: Option<Vec<u8>> = dict.get::<Name<'_>>(b"Subtype").map(|n| n.as_ref().to_vec());
            match subtype.as_deref() {
                Some(b"Image") => {
                    let w = dict.get::<f32>(b"Width").unwrap_or(0.0).max(0.0) as u64;
                    let h = dict.get::<f32>(b"Height").unwrap_or(0.0).max(0.0) as u64;
                    *total = total.saturating_add(image_cost_bytes(w, h, bound_w, bound_h));
                }
                Some(b"Form") => {
                    if let Some(res) = dict.get::<Dict<'_>>(b"Resources") {
                        if let Some(inner) = res.get::<Dict<'_>>(b"XObject") {
                            walk(&inner, depth + 1, bound_w, bound_h, total);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let (pw, ph) = page.render_dimensions();
    let bound_w = (2.0 * scale * pw).ceil().max(0.0) as u64;
    let bound_h = (2.0 * scale * ph).ceil().max(0.0) as u64;
    let mut total = 0u64;
    walk(&page.resources().x_objects, 0, bound_w, bound_h, &mut total);
    total
}

/// Mirror of `estimated_page_bytes` in src/main.rs -- keep them in step.
fn estimated_page_bytes(page: &hayro::hayro_interpret::hayro_syntax::page::Page<'_>, scale: f32) -> u64 {
    PAGE_RENDER_FIXED_BYTES.saturating_add(page_image_bytes(page, scale).saturating_mul(PAGE_IMAGE_FACTOR))
}

/// Mirror of `pick_render_scale` in src/main.rs -- keep them in step.
fn pick_render_scale(fit_scale: f32, mut estimate_fn: impl FnMut(f32) -> u64) -> Option<f32> {
    let mut scale = fit_scale;
    loop {
        if estimate_fn(scale) <= MAX_PAGE_RENDER_BYTES {
            return Some(scale);
        }
        if scale <= fit_scale / 4.0 {
            return None;
        }
        scale /= 2.0;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mem_probe <file.pdf> [more.pdf ...]");
        std::process::exit(2);
    }

    // Staged peaks: knowing WHICH step allocates decides the fix. If parsing
    // dominates, a file-size cap is the only lever; if rendering dominates, we
    // can cap the rasterized area instead and keep opening big documents.
    println!(
        "{:<24} {:>8} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "file", "size MB", "read MB", "parse MB", "render MB", "ratio", "page px"
    );
    for path in &args {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("{path:<28} read failed: {e}");
                continue;
            }
        };
        let file_mb = mb(bytes.len() as u64);
        let base = peak_rss();
        let after_read = peak_rss();

        let pdf = match Pdf::new(Arc::new(bytes)) {
            Ok(p) => p,
            Err(_) => {
                println!("{path:<28} {file_mb:>9.1}  parse failed");
                continue;
            }
        };
        let pages = pdf.pages();
        if pages.is_empty() {
            println!("{path:<28} {file_mb:>9.1}  no pages");
            continue;
        }
        let after_parse = peak_rss();

        // What the app's pre-flight (`estimated_page_bytes` + the
        // `pick_render_scale` retry ladder) would decide for every page:
        // cheap (no rendering), so it answers "which pages would this build
        // actually show, and at what scale?" directly. Scale-aware since the
        // vendored downsample patches make render scale a real cost lever --
        // a page that doesn't fit at fit-width may still be admitted,
        // degraded, at fit/2 or fit/4.
        if std::env::var("PROBE_PAGE_BUDGET").is_ok() {
            let mut refused = 0usize;
            let mut degraded = 0usize;
            let mut worst = 0u64;
            let limit: u64 = std::env::var("PROBE_LIMIT_MB").ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|mb| mb * 1024 * 1024)
                .unwrap_or(MAX_PAGE_RENDER_BYTES);
            for (i, page) in pages.iter().enumerate() {
                let (pw, ph) = page.render_dimensions();
                let mut fit_scale = if pw > 0.0 { PAGE_WIDTH / pw } else { 1.0 };
                if ph * fit_scale > MAX_PAGE_HEIGHT {
                    fit_scale = MAX_PAGE_HEIGHT / ph;
                }
                let picked = pick_render_scale(fit_scale, |s| estimated_page_bytes(page, s));
                match picked {
                    Some(scale) => {
                        let bytes = estimated_page_bytes(page, scale);
                        worst = worst.max(bytes);
                        if bytes > limit {
                            // Fits the app's real MAX_PAGE_RENDER_BYTES but
                            // not this probe's (possibly tighter) PROBE_LIMIT_MB.
                            refused += 1;
                            if refused <= 6 {
                                println!("    page {:>3}: {:>7.1} MB modeled at {:.2}x scale — over PROBE_LIMIT_MB", i + 1, mb(bytes), scale / fit_scale);
                            }
                        } else if scale < fit_scale {
                            degraded += 1;
                            if degraded <= 6 {
                                println!("    page {:>3}: {:>7.1} MB modeled — admitted at {:.2}x scale", i + 1, mb(bytes), scale / fit_scale);
                            }
                        }
                    }
                    None => {
                        let bytes = estimated_page_bytes(page, fit_scale / 4.0);
                        worst = worst.max(bytes);
                        refused += 1;
                        if refused <= 6 {
                            println!("    page {:>3}: {:>7.1} MB modeled even at 1/4 scale — REFUSED", i + 1, mb(bytes));
                        }
                    }
                }
            }
            println!(
                "    pages refused: {} of {} (worst page {:.1} MB); pages degraded (reduced scale): {}",
                refused, pages.len(), mb(worst), degraded
            );
        }

        // Per-page sweep: rendering cost is a property of page CONTENT (a
        // full-resolution embedded photo decodes to RGBA regardless of the
        // scale we draw it at), so the worst page is what has to fit, not the
        // average and not the file size. Note this does NOT mirror the app:
        // it renders every page against ONE parsed `Pdf`, so hayro's
        // in-`Pdf` retention accumulates across the sweep (the app reparses a
        // fresh `Pdf` per page -- see `State::data` in src/main.rs). Use
        // `examples/app_loop_probe.rs` instead for app-faithful peak-RSS
        // numbers.
        if std::env::var("PROBE_ALL_PAGES").is_ok() {
            let mut worst = (0usize, 0u64);
            let mut prev = after_parse;
            for (i, page) in pages.iter().enumerate() {
                let (pw, ph) = page.render_dimensions();
                let mut scale = if pw > 0.0 { PAGE_WIDTH / pw } else { 1.0 };
                if ph * scale > MAX_PAGE_HEIGHT {
                    scale = MAX_PAGE_HEIGHT / ph;
                }
                let cache = RenderCache::new();
                let pix = render(
                    page,
                    &cache,
                    &InterpreterSettings::default(),
                    &RenderSettings { x_scale: scale, y_scale: scale, ..Default::default() },
                );
                let now = peak_rss();
                let delta = now.saturating_sub(prev);
                let imgs = page_image_bytes(page, scale);
                if delta > worst.1 {
                    worst = (i + 1, delta);
                }
                let px_bytes = pix.width() as u64 * pix.height() as u64 * 4;
                drop(pix);
                if delta > 4 * 1024 * 1024 || i < 6 {
                    println!(
                        "    page {:>3}: images {:>6.1} MB -> render +{:>6.1} MB  (x{:.1})  peak {:>6.1}",
                        i + 1, mb(imgs), mb(delta), mb(delta) / mb(imgs).max(0.1), mb(now)
                    );
                }
                let _ = px_bytes;
                prev = now;
            }
            println!(
                "    worst single page: {} (+{:.1} MB); final peak {:.1} MB over {} pages",
                worst.0, mb(worst.1), mb(peak_rss()), pages.len()
            );
        }

        let page = &pages[0];
        let (pw, ph) = page.render_dimensions();
        let mult: f32 = std::env::var("PROBE_SCALE_MULT").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let mut scale = if pw > 0.0 { (PAGE_WIDTH * mult) / pw } else { 1.0 };
        if ph * scale > MAX_PAGE_HEIGHT {
            scale = MAX_PAGE_HEIGHT / ph;
        }
        let cache = RenderCache::new();
        let pix = render(
            page,
            &cache,
            &InterpreterSettings::default(),
            &RenderSettings { x_scale: scale, y_scale: scale, ..Default::default() },
        );
        let px = pix.width() as u64 * pix.height() as u64;

        let peak = peak_rss();
        // `base` is this process's high-water mark BEFORE this file, so run ONE
        // file per process: peak RSS never goes down, and a second file would
        // inherit the first one's mark.
        let used = peak.saturating_sub(base);
        println!(
            "{:<24} {:>8.1} {:>9.1} {:>9.1} {:>9.1} {:>8.1} {:>9}",
            path.rsplit('/').next().unwrap_or(path),
            file_mb,
            mb(after_read),
            mb(after_parse),
            mb(peak),
            if file_mb > 0.0 { mb(used.max(1)) / file_mb } else { 0.0 },
            px,
        );
        drop(pix);
        drop(pdf);
    }
}
