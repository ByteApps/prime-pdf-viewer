mod theme;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;

use hayro::{render, InterpreterSettings, Pdf, RenderSettings};
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
/// Opening a PDF costs far more than the file: at peak the app holds the whole
/// file, hayro's parsed objects and the decompressed images for the page,
/// hayro's pixmap, AND our copy of that pixmap in a `SharedPixelBuffer`.
/// Measured with `cargo run --release --example mem_probe` against hayro 0.4.0
/// (one process per file, peak RSS):
///
/// | file    | peak RSS | over baseline |
/// |---------|----------|---------------|
/// | 0.4 MB  | 12.5 MB  |  2.5 MB       |
/// | 2.1 MB  | 21.6 MB  | 11.6 MB       |
/// | 5.7 MB  | 31.5 MB  | 21.5 MB       |
/// | 13.4 MB | 49.6 MB  | 39.6 MB       |
/// | 32.4 MB | 74.1 MB  | 64.1 MB       |
///
/// i.e. roughly `2.4 x file + 8 MB`, before the app's own UI and framebuffers.
/// An ~11 MB PDF therefore wants ~35-40 MB of heap, which is what killed the
/// app on device (reported 2026-08-17); KeyOS publishes no per-app heap budget
/// for us to read, so this ceiling is empirical, not derived.
///
/// 6 MB predicts a ~22 MB peak — comfortably under the size that crashed, and
/// still covering ordinary documents. RAISE IT ONLY WITH DEVICE EVIDENCE: the
/// failure it prevents is an abort with no error path, not a caught error.
const MAX_PDF_BYTES: u64 = 6 * 1024 * 1024;

/// Mutable app state shared across the UI callbacks.
struct State {
    location: Location,
    path: String,          // current directory, always starts with '/'
    pdf: Option<Pdf>,      // the open document (owns its bytes)
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
        pdf: None,
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
            match Pdf::new(Arc::new(bytes)) {
                Ok(pdf) => {
                    let page_count = pdf.pages().len();
                    if page_count == 0 {
                        show_error(&ui, "This PDF has no pages".to_string());
                        return;
                    }
                    {
                        let mut s = state.borrow_mut();
                        s.pdf = Some(pdf);
                        s.page_idx = 0;
                        s.doc_name = name.to_string();
                    }
                    if show_page(&ui, &state.borrow()) {
                        show_info(&ui, "");
                        ui.global::<Ui>().set_viewing(true);
                    } else {
                        state.borrow_mut().pdf = None;
                        show_error(&ui, "Couldn't render this PDF".to_string());
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
            s.pdf = None;
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
            if s.pdf.is_some() && s.page_idx > 0 {
                s.page_idx -= 1;
                show_page(&ui, &s);
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
            let count = s.pdf.as_ref().map_or(0, |p| p.pages().len());
            if s.page_idx + 1 < count {
                s.page_idx += 1;
                show_page(&ui, &s);
            }
        });
    }

    ui.run().expect("UI running");
}

/// Rasterize the current page fit-to-width and push it into the Viewer global.
/// Returns false if rendering panicked (malformed content stream).
fn show_page(ui: &AppWindow, st: &State) -> bool {
    let Some(pdf) = st.pdf.as_ref() else { return false };
    let pages = pdf.pages();
    let count = pages.len();
    let page = &pages[st.page_idx];

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
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render(
            page,
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
            return false;
        }
    };

    let (w, h) = (pix.width() as u32, pix.height() as u32);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(pix.data_as_u8_slice());

    let viewer = ui.global::<Viewer>();
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
