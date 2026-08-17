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

use hayro::{render, InterpreterSettings, Pdf, RenderSettings};

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mem_probe <file.pdf> [more.pdf ...]");
        std::process::exit(2);
    }

    println!("{:<28} {:>9} {:>11} {:>9} {:>10}", "file", "size MB", "peak RSS MB", "ratio", "page px");
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
        let page = &pages[0];
        let (pw, ph) = page.render_dimensions();
        let mut scale = if pw > 0.0 { PAGE_WIDTH / pw } else { 1.0 };
        if ph * scale > MAX_PAGE_HEIGHT {
            scale = MAX_PAGE_HEIGHT / ph;
        }
        let pix = render(
            page,
            &InterpreterSettings::default(),
            &RenderSettings { x_scale: scale, y_scale: scale, ..Default::default() },
        );
        let px = pix.width() as u64 * pix.height() as u64;

        let peak = peak_rss();
        // `base` is this process's high-water mark BEFORE this file, so the
        // delta understates nothing but does inherit earlier files' peak;
        // report both so a rising baseline is visible rather than hidden.
        let used = peak.saturating_sub(base);
        println!(
            "{:<28} {:>9.1} {:>11.1} {:>9.1} {:>10}",
            path.rsplit('/').next().unwrap_or(path),
            file_mb,
            mb(peak),
            if file_mb > 0.0 { mb(used.max(1)) / file_mb } else { 0.0 },
            px,
        );
        drop(pix);
        drop(pdf);
    }
}
