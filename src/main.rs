mod theme;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;

use hayro::hayro_interpret::hayro_syntax::object::{Dict, Stream};
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::{render, RenderCache, RenderSettings};
use slint_keyos_platform::app_ui2;
use slint_keyos_platform::fs::{self, Location, OpenFlags};
use slint_keyos_platform::slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
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

            log::info!("cb: open-pdf {name}");
            let bytes = match read_bytes(&fs, &full, loc) {
                Ok(b) => b,
                Err(msg) => {
                    show_error(&ui, msg);
                    return;
                }
            };
            let data = Arc::new(bytes);
            match Pdf::new(data.clone()) {
                Ok(pdf) => {
                    let page_count = pdf.pages().len();
                    if page_count == 0 {
                        show_error(&ui, "This PDF has no pages".to_string());
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
        });
    }

    // Back button: go up one directory.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_go_back(move || {
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
            let mut s = state.borrow_mut();
            if s.data.is_some() && s.page_idx > 0 {
                s.page_idx -= 1;
                show_page(&ui, &s); // false here just means "refused"; stay open
            }
        });
    }
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_next_page(move || {
            log::info!("cb: next-page");
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            let count = s.page_count;
            if s.page_idx + 1 < count {
                s.page_idx += 1;
                show_page(&ui, &s); // false here just means "refused"; stay open
            }
        });
    }

    ui.run().expect("UI running");
}

/// Budget for rendering ONE page, and the model used to predict it.
///
/// Measured per page with `examples/mem_probe` (`PROBE_ALL_PAGES=1`) against
/// the 11 MB owner's manual that crashed the device:
///
/// | page | images   | render cost |
/// |------|----------|-------------|
/// | 1    | 18.2 MB  | +53.4 MB    |
/// | 3, 4 |  0.0 MB  | + 8.2 MB    |
/// | 5-9  |  1.5 MB  | + 8.1 MB    |
///
/// Two components: a FIXED ~8 MB per page (fonts, content streams, hayro's
/// per-page state) that appears even on pages with no images at all, plus
/// roughly 3x the decoded image bytes. Summing image bytes alone was not
/// enough -- it predicted 18 MB for a page that cost 53 MB, and would have
/// waved this document through.
const PAGE_RENDER_FIXED_BYTES: u64 = 8 * 1024 * 1024;
/// Multiplier on decoded image bytes. hayro holds more than one representation
/// while drawing (decode, colour conversion, scaling), so 4 bytes/pixel of
/// source image costs ~3x that at peak.
const PAGE_IMAGE_FACTOR: u64 = 3;
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

/// Predict what rendering this page will cost: the fixed per-page overhead
/// plus a multiple of its decoded image bytes. See the constants above for the
/// measurements behind both terms.
fn estimated_page_bytes(page: &Page<'_>) -> u64 {
    PAGE_RENDER_FIXED_BYTES
        .saturating_add(page_image_bytes(page).saturating_mul(PAGE_IMAGE_FACTOR))
}

/// Sum the decoded size of every image a page draws, following Form XObjects.
///
/// Deliberately an OVER-estimate of what hayro will hold: it counts every
/// image the page references, at 4 bytes per pixel, without deduplicating
/// repeats. Under-counting here would let through exactly the page that
/// aborts the process, so the error direction is chosen on purpose.
fn page_image_bytes(page: &Page<'_>) -> u64 {
    fn walk(xobjects: &Dict<'_>, depth: u32, total: &mut u64) {
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
                    // 4 bytes/px: hayro decodes to RGBA8 regardless of the
                    // source colour space or bit depth.
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

    // Pre-flight: refuse a page we cannot afford to draw, BEFORE hayro tries.
    // A file-size cap cannot catch this -- an 11 MB manual needs ~54-87 MB for
    // page 1 while a 13 MB image PDF needs ~50 MB -- because the cost is the
    // page's images decoded at NATIVE resolution, which no render scale
    // reduces (verified: 1/4 scale changed the peak by 4%).
    let predicted = estimated_page_bytes(page);
    if predicted > MAX_PAGE_RENDER_BYTES {
        log::warn!(
            "page {} needs ~{} to render (limit {})",
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
        // Keep the document open on a refused page: the rest of it is usually
        // fine (290 of the crashing manual's 291 pages cost ~8 MB), so the
        // reader can page past this one instead of being thrown out.
        let viewer = ui.global::<Viewer>();
        viewer.set_page_img(Image::default());
        viewer.set_page_h(0.0);
        viewer.set_page_message(msg.into());
        viewer.set_page_num(st.page_idx as i32 + 1);
        viewer.set_page_count(count as i32);
        viewer.set_doc_name(st.doc_name.clone().into());
        return false;
    }

    let (pw, ph) = page.render_dimensions();
    let mut scale = if pw > 0.0 { PAGE_WIDTH / pw } else { 1.0 };
    if ph * scale > MAX_PAGE_HEIGHT {
        scale = MAX_PAGE_HEIGHT / ph;
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

    let viewer = ui.global::<Viewer>();
    viewer.set_page_message("".into());
    viewer.set_page_img(Image::from_rgba8_premultiplied(buf));
    viewer.set_page_h(h as f32);
    viewer.set_page_num(st.page_idx as i32 + 1);
    viewer.set_page_count(count as i32);
    viewer.set_doc_name(st.doc_name.clone().into());
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
