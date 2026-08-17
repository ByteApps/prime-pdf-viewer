mod theme;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use hayro::hayro_interpret::hayro_syntax::object::{Dict, Stream};
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};
use slint_keyos_platform::app_ui2;
use slint_keyos_platform::fs::{self, Location, OpenFlags};
use slint_keyos_platform::slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, VecModel,
};

app_ui2!("PDF Viewer");

/// Width the page is rasterized to: the window (480) minus the 20px content
/// padding on each side, so a page always fits the screen edge to edge.
const PAGE_WIDTH: f32 = 440.0;
/// Cap on rendered pixmap height, to bound the allocation for absurd pages.
const MAX_PAGE_HEIGHT: f32 = 4096.0;

/// Largest PDF we will try to open.
///
/// The FILE is the cheap part: holding the bytes plus hayro's parsed objects
/// costs about 1.6x the file size (an 11 MB manual reaches ~19 MB after
/// parsing). What actually blows the heap is rendering a page, which
/// `estimated_page_bytes` gates separately -- so this cap only has to keep the
/// file itself affordable, and should NOT be tightened to compensate for
/// expensive pages. Doing that refuses whole documents whose pages are almost
/// all cheap: 290 of that manual's 291 pages need ~8 MB.
const MAX_PDF_BYTES: u64 = 16 * 1024 * 1024;

/// Mutable app state shared across the UI callbacks.
struct State {
    location: Location,
    path: String,          // current directory, always starts with '/'
    /// The open document's BYTES, not a parsed `Pdf`.
    ///
    /// hayro caches decoded page content inside the `Pdf` and never evicts it:
    /// measured with `examples/mem_probe`, each rendered page retains ~9 MB
    /// that live RSS confirms is never returned (299 MB after 25 pages of an
    /// 11 MB manual). Keeping one `Pdf` for the session therefore grows without
    /// bound as the user pages. We reparse per render instead -- parsing is
    /// ~6 MB and fast -- so a session's peak is one page's cost, not the sum.
    data: Option<Arc<Vec<u8>>>,
    page_count: usize,
    page_idx: usize,       // 0-based index of the page on screen
    doc_name: String,
    /// True while a deferred open/page-turn is in flight. Guards against a
    /// repeated tap re-entering the (slow, synchronous-once-it-starts) work
    /// and queuing up; see the loading overlay in ui/app.slint.
    busy: bool,
}

fn app_main(cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    theme::init(&ui);

    let fs = cx.fs.clone();
    let ui_weak = ui.as_weak();
    let state = Rc::new(RefCell::new(State {
        location: Location::User,
        path: "/".to_string(),
        data: None,
        page_count: 0,
        page_idx: 0,
        doc_name: String::new(),
        busy: false,
    }));

    // Re-list the current directory (folders + .pdf files) into the Browser global.
    let refresh: Rc<dyn Fn()> = {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        Rc::new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, path) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let browser = ui.global::<Browser>();

            let mut items: Vec<(bool, String, String)> = Vec::new();
            let mut status = String::new();
            match fs.open_dir(path.as_str(), loc) {
                Ok(dir) => loop {
                    match dir.next_entry() {
                        Ok(Some(entry)) => {
                            if entry.name.starts_with('.') {
                                continue; // includes "." and ".."
                            }
                            if entry.is_dir {
                                items.push((true, entry.name, "Folder".to_string()));
                            } else if entry.name.to_lowercase().ends_with(".pdf") {
                                items.push((false, entry.name, human_size(entry.len)));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            status = err_msg(&e);
                            break;
                        }
                    }
                },
                Err(e) => status = err_msg(&e),
            }

            // Folders first, then alphabetical (case-insensitive).
            items.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });

            let rows: Vec<FileRow> = items
                .into_iter()
                .map(|(is_dir, name, info)| FileRow {
                    name: name.into(),
                    info: info.into(),
                    is_folder: is_dir,
                })
                .collect();

            browser.set_entries(ModelRc::new(VecModel::from(rows)));
            browser.set_path(path.clone().into());
            browser.set_at_root(path == "/");
            browser.set_status(status.into());
        })
    };

    // Populate the initial (Internal) listing.
    refresh();

    let callbacks = ui.global::<Callbacks>();

    // Switch storage tab: Internal / Airlock / USB. Resets to that root.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_location_changed(move |idx| {
            {
                let mut s = state.borrow_mut();
                s.location = location_for(idx);
                s.path = "/".to_string();
            }
            refresh();
        });
    }

    // Tap a row: descend into a folder, or open a PDF in the viewer.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_entry_activated(move |name, is_folder| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let full = join_path(&dir, name.as_str());

            if is_folder {
                state.borrow_mut().path = full;
                refresh();
                return;
            }

            if state.borrow().busy {
                return;
            }

            log::info!("cb: open-pdf {name}");
            state.borrow_mut().busy = true;
            let u = ui.global::<Ui>();
            u.set_loading(true);
            u.set_loading_text(format!("Opening {name}…").into());
            log::info!("loading: {name}");

            // Defer the actual open so this frame paints the overlay first,
            // then do the whole (slow, synchronous) open on the next tick.
            let fs = fs.clone();
            let ui_weak = ui_weak.clone();
            let state = state.clone();
            let name = name.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };

                let finish = |ui: &AppWindow, state: &Rc<RefCell<State>>| {
                    state.borrow_mut().busy = false;
                    ui.global::<Ui>().set_loading(false);
                    log::info!("loading done");
                };

                let bytes = match read_bytes(&fs, &full, loc) {
                    Ok(b) => b,
                    Err(msg) => {
                        show_error(&ui, msg);
                        finish(&ui, &state);
                        return;
                    }
                };
                let data = Arc::new(bytes);
                match Pdf::new(data.clone()) {
                    Ok(pdf) => {
                        let page_count = pdf.pages().len();
                        if page_count == 0 {
                            show_error(&ui, "This PDF has no pages".to_string());
                            finish(&ui, &state);
                            return;
                        }
                        // Drop the parsed document; `show_page` reparses. Holding
                        // it here would keep every page we then render.
                        drop(pdf);
                        {
                            let mut s = state.borrow_mut();
                            s.data = Some(data);
                            s.page_count = page_count;
                            s.page_idx = 0;
                            s.doc_name = name.to_string();
                        }
                        let rendered = show_page(&ui, &state.borrow());
                        let refused_page = !ui.global::<Viewer>().get_page_message().is_empty();
                        if rendered || refused_page {
                            // A refused FIRST page is not a refused document.
                            show_info(&ui, "");
                            ui.global::<Ui>().set_viewing(true);
                        } else {
                            state.borrow_mut().data = None;
                            show_error(&ui, RENDER_REFUSED.with(|m| m.borrow().clone()));
                        }
                    }
                    Err(e) => {
                        log::warn!("open-pdf failed: {e:?}");
                        show_error(
                            &ui,
                            "Couldn't open this file. It may not be a PDF, or it may be encrypted."
                                .to_string(),
                        );
                    }
                }
                finish(&ui, &state);
            });
        });
    }

    // Back button: go up one directory.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_go_back(move || {
            if state.borrow().busy {
                return;
            }
            {
                let mut s = state.borrow_mut();
                s.path = parent_path(&s.path);
            }
            refresh();
        });
    }

    // Leave the viewer and drop the document.
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_close_viewer(move || {
            if state.borrow().busy {
                return;
            }
            log::info!("cb: close-viewer");
            let mut s = state.borrow_mut();
            s.data = None;
            s.page_count = 0;
            s.page_idx = 0;
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Ui>().set_viewing(false);
            }
        });
    }

    // Previous / next page.
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_prev_page(move || {
            log::info!("cb: prev-page");
            let Some(ui) = ui_weak.upgrade() else { return };
            if state.borrow().busy {
                return;
            }
            let (can_go, target_num) = {
                let s = state.borrow();
                (s.data.is_some() && s.page_idx > 0, s.page_idx)
            };
            if !can_go {
                return;
            }
            state.borrow_mut().busy = true;
            let u = ui.global::<Ui>();
            u.set_loading(true);
            u.set_loading_text(format!("Page {target_num}…").into());
            log::info!("loading: page {target_num}");

            let ui_weak = ui_weak.clone();
            let state = state.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                {
                    let mut s = state.borrow_mut();
                    s.page_idx -= 1;
                }
                show_page(&ui, &state.borrow()); // false here just means "refused"; stay open
                state.borrow_mut().busy = false;
                ui.global::<Ui>().set_loading(false);
                log::info!("loading done");
            });
        });
    }
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_next_page(move || {
            log::info!("cb: next-page");
            let Some(ui) = ui_weak.upgrade() else { return };
            if state.borrow().busy {
                return;
            }
            let (can_go, target_num) = {
                let s = state.borrow();
                (s.page_idx + 1 < s.page_count, s.page_idx + 2)
            };
            if !can_go {
                return;
            }
            state.borrow_mut().busy = true;
            let u = ui.global::<Ui>();
            u.set_loading(true);
            u.set_loading_text(format!("Page {target_num}…").into());
            log::info!("loading: page {target_num}");

            let ui_weak = ui_weak.clone();
            let state = state.clone();
            Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                {
                    let mut s = state.borrow_mut();
                    s.page_idx += 1;
                }
                show_page(&ui, &state.borrow()); // false here just means "refused"; stay open
                state.borrow_mut().busy = false;
                ui.global::<Ui>().set_loading(false);
                log::info!("loading done");
            });
        });
    }

    ui.run().expect("UI running");
}

/// Budget for rendering ONE page, and the model used to predict it.
///
/// PRE-PATCH HISTORY (kept for context, no longer the live model): measured
/// per page with `examples/mem_probe` (`PROBE_ALL_PAGES=1`) against the 11 MB
/// owner's manual that crashed the device, BEFORE the vendored hayro
/// downsample patches (`vendor/hayro-syntax`, `vendor/hayro-interpret`):
///
/// | page | images   | render cost |
/// |------|----------|-------------|
/// | 1    | 18.2 MB  | +53.4 MB    |
/// | 3, 4 |  0.0 MB  | + 8.2 MB    |
/// | 5-9  |  1.5 MB  | + 8.1 MB    |
///
/// Two components: a FIXED ~8 MB per page (fonts, content streams, hayro's
/// per-page state) that appears even on pages with no images at all, plus a
/// multiple of the decoded image bytes. Summing image bytes alone was not
/// enough -- it predicted 18 MB for a page that cost 53 MB, and would have
/// waved this document through. At that time render SCALE was NOT a lever:
/// 1/4 scale (16x fewer output pixels) changed the peak by only 4%, because
/// hayro decoded every image at its NATIVE resolution regardless of how small
/// it was drawn.
///
/// POST-PATCH (current model): the two vendored patches make every raster
/// image decode toward at most ~2x its DRAWN size (DCTDecode scales at the
/// syntax level; every other filter is downsampled after decode). Re-measured
/// on the same manual, page 1 (18.2 MB of dict-modeled image bytes, mostly
/// small Flate bitmaps) now costs +31 MB to render at fit-width scale -- a
/// multiplier of ~1.7x on dict-modeled image bytes, down from the pre-patch
/// ~2.9x. Render scale is now a REAL lever: at half scale, images draw at
/// half size, so decoded bytes quarter (the old "scale doesn't matter" claim
/// above is history, not current behaviour). `page_image_bytes` and
/// `estimated_page_bytes` are scale-aware accordingly, and `show_page` uses
/// that to retry a too-expensive page at reduced scale instead of refusing
/// outright.
const PAGE_RENDER_FIXED_BYTES: u64 = 8 * 1024 * 1024;
/// Multiplier on decoded image bytes. hayro holds more than one representation
/// while drawing (decode, colour conversion, scaling), so 4 bytes/pixel of
/// source image costs some multiple of that at peak. Recalibrated from 3 to 2
/// post-patch: measured ~1.7x on the manual's worst (image-heaviest) page,
/// plus headroom.
const PAGE_IMAGE_FACTOR: u64 = 2;
/// Refuse a page whose predicted cost exceeds this.
///
/// EMPIRICAL, pending device calibration (`pdf-calibration/` on the USB
/// stick): KeyOS exposes no per-app heap budget to read. 32 MB admits every
/// ordinary page -- including 290 of the manual's 291 -- and refuses its
/// image-heavy cover, which is the page that actually crashed the app.
const MAX_PAGE_RENDER_BYTES: u64 = 32 * 1024 * 1024;

thread_local! {
    /// Why the last render was refused. `show_page` returns a bare bool to its
    /// callers (three call sites, one of which is inside a borrow), so the
    /// explanation rides here rather than through every signature.
    static RENDER_REFUSED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_refusal(msg: &str) {
    RENDER_REFUSED.with(|m| *m.borrow_mut() = msg.to_string());
}

/// Predict what rendering this page will cost AT THE GIVEN RENDER SCALE: the
/// fixed per-page overhead plus a multiple of its decoded image bytes. See
/// the constants above for the measurements behind both terms.
fn estimated_page_bytes(page: &Page<'_>, scale: f32) -> u64 {
    PAGE_RENDER_FIXED_BYTES
        .saturating_add(page_image_bytes(page, scale).saturating_mul(PAGE_IMAGE_FACTOR))
}

/// The decode-cost cap the vendored downsample patches guarantee for one
/// image: it cannot land more than ~2x above `(bound_w, bound_h)`, the size
/// it's actually drawn at. Cost is `4 bytes/px * min(native area, bound
/// area)` -- the smaller of "decode this image at its own native resolution"
/// and "decode it at the bound the patches enforce", since hayro never pays
/// for more than either.
fn image_cost_bytes(native_w: u64, native_h: u64, bound_w: u64, bound_h: u64) -> u64 {
    let native_area = native_w.saturating_mul(native_h);
    let bound_area = bound_w.saturating_mul(bound_h);
    4u64.saturating_mul(native_area.min(bound_area))
}

/// Sum the decoded size of every image a page draws, following Form XObjects.
///
/// Deliberately an OVER-estimate of what hayro will hold: it counts every
/// image the page references, without deduplicating repeats, and bounds each
/// one's decode cost by the whole PAGE's pixmap at this scale (`2 * scale *
/// render_dimensions`) rather than that image's own placement within the
/// page -- we don't walk the content stream to know an image's actual drawn
/// footprint, only that it can't exceed the page it's drawn on. The `2x`
/// mirrors the vendored patches' own margin: they downsample toward the
/// drawn size but tolerate up to ~2x that before touching a decode.
/// Under-counting here would let through exactly the page that aborts the
/// process, so the error direction is chosen on purpose.
fn page_image_bytes(page: &Page<'_>, scale: f32) -> u64 {
    fn walk(xobjects: &Dict<'_>, depth: u32, bound_w: u64, bound_h: u64, total: &mut u64) {
        // Forms can nest; bound the recursion rather than trusting the file.
        if depth > 4 {
            return;
        }
        for key in xobjects.keys() {
            let Some(stream) = xobjects.get::<Stream<'_>>(key.as_ref()) else { continue };
            let dict = stream.dict();
            let subtype: Option<Vec<u8>> =
                dict.get::<hayro::hayro_interpret::hayro_syntax::object::Name<'_>>(b"Subtype")
                    .map(|n| n.as_ref().to_vec());
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

/// Pick the largest render scale, from `fit_scale` down to `fit_scale / 4` in
/// halving steps, whose `estimate_fn(scale)` fits `MAX_PAGE_RENDER_BYTES`.
/// Returns `None` if even the floor (`fit_scale / 4`) doesn't fit -- the
/// caller refuses the page in that case. Pulled out as a pure function so the
/// ladder logic is testable without a real `Page`.
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

/// Rasterize the current page fit-to-width and push it into the Viewer global.
/// Returns false if rendering panicked (malformed content stream).
fn show_page(ui: &AppWindow, st: &State) -> bool {
    let Some(data) = st.data.as_ref() else { return false };
    // Reparse per render so hayro's per-page cache dies with this `Pdf`
    // (see `State::data`). Costs ~6 MB and a fraction of a second.
    let Ok(pdf) = Pdf::new(data.clone()) else {
        set_refusal("Couldn't read this PDF");
        return false;
    };
    let pages = pdf.pages();
    let count = pages.len();
    if st.page_idx >= count {
        return false;
    }
    let page = &pages[st.page_idx];

    let (pw, ph) = page.render_dimensions();
    let mut fit_scale = if pw > 0.0 { PAGE_WIDTH / pw } else { 1.0 };
    if ph * fit_scale > MAX_PAGE_HEIGHT {
        fit_scale = MAX_PAGE_HEIGHT / ph;
    }

    // Pre-flight: refuse a page we cannot afford to draw, BEFORE hayro tries.
    // A file-size cap cannot catch this -- an 11 MB manual needs ~54-87 MB for
    // page 1 while a 13 MB image PDF needs ~50 MB. Since the vendored hayro
    // patches bound every image's decode to ~2x its DRAWN size, render scale
    // is now a real lever (unlike before those patches, where 1/4 scale
    // changed the peak by only 4%): retry at half, then quarter, fit-width
    // scale before giving up.
    let scale = match pick_render_scale(fit_scale, |s| estimated_page_bytes(page, s)) {
        Some(s) => s,
        None => {
            let predicted = estimated_page_bytes(page, fit_scale / 4.0);
            log::warn!(
                "page {} needs ~{} to render even at 1/4 scale (limit {})",
                st.page_idx + 1,
                human_size(predicted),
                human_size(MAX_PAGE_RENDER_BYTES)
            );
            let msg = format!(
                "Page {} is too detailed to display (needs ~{}).",
                st.page_idx + 1,
                human_size(predicted)
            );
            set_refusal(&msg);
            // Keep the document open on a refused page: the rest of it is
            // usually fine (290 of the crashing manual's 291 pages cost ~8
            // MB), so the reader can page past this one instead of being
            // thrown out.
            let viewer = ui.global::<Viewer>();
            viewer.set_page_img(Image::default());
            viewer.set_page_h(0.0);
            viewer.set_page_message(msg.into());
            viewer.set_page_num(st.page_idx as i32 + 1);
            viewer.set_page_count(count as i32);
            viewer.set_doc_name(st.doc_name.clone().into());
            return false;
        }
    };
    if scale < fit_scale {
        // Log contract: "page {n} degraded to {frac}x scale (~{bytes}
        // modeled)", frac formatted to 2 decimals (0.50, 0.25) relative to
        // fit-width scale.
        log::info!(
            "page {} degraded to {:.2}x scale (~{} modeled)",
            st.page_idx + 1,
            scale / fit_scale,
            human_size(estimated_page_bytes(page, scale))
        );
    }

    // Drop the page currently on screen BEFORE rasterizing the next one.
    // Otherwise peak memory holds three full-page buffers at once: the old
    // page's `Image`, hayro's new pixmap, and our copy of it. At
    // MAX_PAGE_HEIGHT that is ~7 MB each, and the app is already tight enough
    // on heap that an 11 MB document crashed it.
    ui.global::<Viewer>().set_page_img(Image::default());

    // hayro forbids unsafe and normally renders malformed content as blanks,
    // but a panic here would take the whole app down — contain it.
    // hayro 0.7 takes the render cache explicitly. A fresh one per page keeps
    // nothing between renders -- the whole point of reparsing above.
    let cache = RenderCache::new();
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render(
            page,
            &cache,
            &InterpreterSettings::default(),
            &RenderSettings {
                x_scale: scale,
                y_scale: scale,
                // Paper is white. hayro's default bg_color is TRANSPARENT
                // (0.4 rendered opaque, 0.7 does not), which composited the
                // page over the app's dark surface and made scanned/vector
                // pages look inverted. Set it explicitly so the page looks the
                // same whatever the library's default does next.
                bg_color: WHITE,
                ..Default::default()
            },
        )
    }));
    let pix = match rendered {
        Ok(p) => p,
        Err(_) => {
            log::warn!("render panicked on page {}", st.page_idx + 1);
            set_refusal("Couldn't render this page");
            return false;
        }
    };

    let (w, h) = (pix.width() as u32, pix.height() as u32);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(pix.data_as_u8_slice());

    // The Slint side maps pixmap pixels 1:1 (Image is `width: parent.width;
    // image-fit: fill`, and `page_h` sets its height directly). At a
    // degraded (< fit-width) scale the pixmap is narrower than PAGE_WIDTH,
    // so `page_h` must be the DISPLAY height the pixmap scales UP to at
    // PAGE_WIDTH, not the pixmap's own height -- otherwise a degraded page
    // renders squashed. At fit-width scale (pixmap_w == PAGE_WIDTH) this is a
    // no-op.
    let display_h = if w > 0 {
        h as f32 * (PAGE_WIDTH / w as f32)
    } else {
        h as f32
    };

    let viewer = ui.global::<Viewer>();
    viewer.set_page_message("".into());
    viewer.set_page_img(Image::from_rgba8_premultiplied(buf));
    viewer.set_page_h(display_h);
    viewer.set_page_num(st.page_idx as i32 + 1);
    viewer.set_page_count(count as i32);
    viewer.set_doc_name(st.doc_name.clone().into());
    // Log contract: keeps logging the PIXMAP dims (w x h), not the display
    // size -- ../ui-automation/tests/view-pdf.sh greps this line.
    log::info!("rendered page {}/{} {}x{}", st.page_idx + 1, count, w, h);
    true
}

/// Read a whole file; returns a user-facing message on failure.
///
/// Refuses anything over `MAX_PDF_BYTES` before allocating, and reserves the
/// buffer fallibly: a plain `read_to_end` grows by doubling and ABORTS the
/// process if an allocation fails, which is a crash with no error path and no
/// log line. `try_reserve_exact` turns that into a message instead, and asking
/// for the exact size also avoids the doubling overshoot (a 6 MB file can
/// otherwise transiently hold an 8 MB buffer).
fn read_bytes(
    fs: &fs::FileSystem<fs_permissions::FileSystemPermissions>,
    path: &str,
    loc: Location,
) -> Result<Vec<u8>, String> {
    let mut file = fs
        .open_file(path, loc, OpenFlags::READ_ONLY)
        .map_err(|e| err_msg(&e))?;
    let len = file.metadata().map(|m| m.size).unwrap_or(0);
    if len > MAX_PDF_BYTES {
        return Err(format!(
            "This PDF is {} — too large to open (limit {}).",
            human_size(len),
            human_size(MAX_PDF_BYTES)
        ));
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(len as usize)
        .map_err(|_| "Not enough memory to open this PDF".to_string())?;
    file.read_to_end(&mut buf)
        .map_err(|_| "Read failed".to_string())?;
    Ok(buf)
}

fn show_info(ui: &AppWindow, msg: &str) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(false);
}

fn show_error(ui: &AppWindow, msg: String) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(true);
}

fn location_for(index: i32) -> Location {
    match index {
        1 => Location::Airlock,
        2 => Location::Usb,
        _ => Location::User,
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn err_msg(e: &fs::Error) -> String {
    use slint_keyos_platform::fs::Error::*;
    match e {
        NoMedia => "Not connected".to_string(),
        AccessDenied => "Access denied".to_string(),
        FileNotFound => "Not found".to_string(),
        FileAlreadyExists => "Already exists".to_string(),
        FileInUse => "File is in use".to_string(),
        InvalidPath => "Invalid name".to_string(),
        other => format!("Error: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- image_cost_bytes -----------------------------------------------

    #[test]
    fn image_cost_native_smaller_than_bound_wins() {
        // A small native image inside a page bound far larger than it: the
        // cost should be the native size, not the bound.
        let cost = image_cost_bytes(100, 100, 10_000, 10_000);
        assert_eq!(cost, 4 * 100 * 100);
    }

    #[test]
    fn image_cost_bound_smaller_than_native_wins() {
        // A huge native image drawn small: the vendored patches cap its
        // decode near the bound, so the bound should win.
        let cost = image_cost_bytes(4000, 3000, 200, 150);
        assert_eq!(cost, 4 * 200 * 150);
    }

    #[test]
    fn image_cost_equal_dimensions_either_bound_works() {
        let cost = image_cost_bytes(500, 500, 500, 500);
        assert_eq!(cost, 4 * 500 * 500);
    }

    // -- pick_render_scale ------------------------------------------------

    #[test]
    fn pick_render_scale_fits_at_full_scale() {
        // Cheap page: fits without any degradation.
        let got = pick_render_scale(1.0, |_s| 1_000);
        assert_eq!(got, Some(1.0));
    }

    #[test]
    fn pick_render_scale_degrades_to_half_when_only_half_fits() {
        // DISCRIMINATING: an estimate that only fits at fit/2 must yield
        // exactly fit/2 -- this fails if the ladder skips a step, or if the
        // estimate function isn't actually scale-aware (a constant estimate
        // would either always pass or always fail, never split like this).
        let fit = 1.0f32;
        let got = pick_render_scale(fit, |s| {
            if s <= fit / 2.0 + f32::EPSILON {
                MAX_PAGE_RENDER_BYTES // exactly at the limit: fits
            } else {
                MAX_PAGE_RENDER_BYTES + 1 // over the limit: refused
            }
        });
        assert_eq!(got, Some(0.5));
    }

    #[test]
    fn pick_render_scale_degrades_to_quarter_when_only_quarter_fits() {
        let fit = 2.0f32;
        let got = pick_render_scale(fit, |s| {
            if s <= fit / 4.0 + f32::EPSILON {
                MAX_PAGE_RENDER_BYTES
            } else {
                MAX_PAGE_RENDER_BYTES + 1
            }
        });
        assert_eq!(got, Some(0.5)); // fit/4 == 0.5
    }

    #[test]
    fn pick_render_scale_refuses_when_floor_still_too_big() {
        // Nothing in the ladder ever fits: refuse.
        let got = pick_render_scale(1.0, |_s| MAX_PAGE_RENDER_BYTES + 1);
        assert_eq!(got, None);
    }
}
