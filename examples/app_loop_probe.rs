//! Measure the app's EXACT per-page loop -- app-faithful peak RSS, unlike
//! `mem_probe`'s `PROBE_ALL_PAGES` sweep (which renders every page against
//! ONE parsed `Pdf` and so accumulates hayro's in-`Pdf` retention across the
//! whole sweep; see its doc comment). This mirrors `show_page` in
//! `src/main.rs`: reparse a fresh `Pdf` from the same `Arc<Vec<u8>>` and use
//! a fresh `RenderCache` for every single page, exactly as `State::data` +
//! `show_page` do on-device -- so whatever this reports IS what a real
//! session's peak looks like, not an upper bound inflated by cross-page
//! retention.
//!
//! Cycles through 30 renders (page `i % page_count`, so short documents wrap
//! around and still exercise 30 reparse+render cycles), fit-to-width scale,
//! reporting peak RSS every 5 pages.
//!
//! CONTROL RESULT (main branch, before the vendored hayro downsample
//! patches, against the 11 MB Owners_Manual fixture): peak RSS climbs ~8
//! MB/page with NO plateau -- 301.7 MB by page 30. That climb is pre-existing
//! (it is NOT introduced by the scale-aware cost model / retry-at-reduced-
//! scale work in this branch) and is presumably hayro-internal steady-state
//! allocator growth that isn't page-content-retention in the `Pdf` (each
//! iteration reparses fresh), since `show_page`'s per-page reparse was
//! already supposed to prevent unbounded growth.
//!
//! WITH the vendored patches (this branch: `vendor/hayro-syntax`'s
//! target_dimension-aware DCTDecode + `vendor/hayro-interpret`'s generic
//! raster downsample, both bounding decode toward ~2x drawn size): the same
//! run PLATEAUS at ~196 MB by page 20 instead of climbing unbounded. The
//! patches don't eliminate the underlying growth, but they substantially
//! improve it -- smaller per-page decodes mean less to retain/reallocate
//! before the process reaches a steady state.
//!
//!   cargo run --release --example app_loop_probe -- <file.pdf>
//!
//! Run on the HOST. One file per invocation (peak RSS is a high-water mark
//! that never falls, so a second file would inherit the first's peak).
use std::sync::Arc;

/// Peak resident set size in bytes, as the kernel measured it for this
/// process. `ru_maxrss` is a high-water mark, so it never under-reports a
/// transient spike the way periodic sampling would.
fn peak_rss() -> u64 {
    #[repr(C)]
    struct RUsage {
        ru_utime: [u64; 2],
        ru_stime: [u64; 2],
        ru_maxrss: i64,
        rest: [i64; 13],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }
    let mut u = RUsage { ru_utime: [0; 2], ru_stime: [0; 2], ru_maxrss: 0, rest: [0; 13] };
    // RUSAGE_SELF == 0. macOS reports ru_maxrss in BYTES (Linux: kilobytes).
    unsafe { getrusage(0, &mut u) };
    if cfg!(target_os = "macos") { u.ru_maxrss as u64 } else { u.ru_maxrss as u64 * 1024 }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: app_loop_probe <file.pdf>");
        std::process::exit(2);
    });
    let bytes = Arc::new(std::fs::read(&path).expect("read PDF"));

    for i in 0..30 {
        // Mirrors show_page's exact per-page loop: reparse a fresh `Pdf` and
        // use a fresh `RenderCache` for every render, so nothing survives
        // between iterations except the shared byte buffer -- exactly what
        // `State::data` retains on-device.
        let pdf = hayro::hayro_interpret::hayro_syntax::Pdf::new(bytes.clone()).unwrap();
        let pages = pdf.pages();
        let page = &pages[i % pages.len()];
        let (pw, _ph) = page.render_dimensions();
        let scale = if pw > 0.0 { 440.0 / pw } else { 1.0 };
        let cache = hayro::RenderCache::new();
        let pix = hayro::render(
            page,
            &cache,
            &hayro::hayro_interpret::InterpreterSettings::default(),
            &hayro::RenderSettings { x_scale: scale, y_scale: scale, ..Default::default() },
        );
        let _ = pix.width();
        if i % 5 == 4 {
            println!("after page {:>2}: peak {:.1} MB", i + 1, peak_rss() as f64 / 1_048_576.0);
        }
    }
}
