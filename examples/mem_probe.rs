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

/// Mirror of `page_image_bytes` in src/main.rs -- keep them in step.
fn page_image_bytes(page: &hayro::hayro_interpret::hayro_syntax::page::Page<'_>) -> u64 {
    use hayro::hayro_interpret::hayro_syntax::object::{Dict, Name, Stream};
    fn walk(xobjects: &Dict<'_>, depth: u32, total: &mut u64) {
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
                    *total = total.saturating_add(w.saturating_mul(h).saturating_mul(4));
                }
                Some(b"Form") => {
                    if let Some(res) = dict.get::<Dict<'_>>(b"Resources") {
                        if let Some(inner) = res.get::<Dict<'_>>(b"XObject") {
                            walk(&inner, depth + 1, total);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut total = 0u64;
    walk(&page.resources().x_objects, 0, &mut total);
    total
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

        // What the app's pre-flight would compute for every page: the sum of
        // image XObjects at w*h*4. Cheap (no rendering), so it answers "which
        // pages would this build actually show?" directly.
        if std::env::var("PROBE_PAGE_BUDGET").is_ok() {
            let mut over = 0usize;
            let mut worst = 0u64;
            let limit: u64 = std::env::var("PROBE_LIMIT_MB").ok()
                .and_then(|v| v.parse::<u64>().ok()).unwrap_or(24) * 1024 * 1024;
            for (i, page) in pages.iter().enumerate() {
                let bytes = page_image_bytes(page);
                worst = worst.max(bytes);
                if bytes > limit {
                    over += 1;
                    if over <= 6 {
                        println!("    page {:>3}: {:>7.1} MB of images — REFUSED", i + 1, mb(bytes));
                    }
                }
            }
            println!(
                "    pages over {} MB: {} of {} (worst page {:.1} MB)",
                limit / 1024 / 1024, over, pages.len(), mb(worst)
            );
        }

        // Per-page sweep: rendering cost is a property of page CONTENT (a
        // full-resolution embedded photo decodes to RGBA regardless of the
        // scale we draw it at), so the worst page is what has to fit, not the
        // average and not the file size.
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
                let imgs = page_image_bytes(page);
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
