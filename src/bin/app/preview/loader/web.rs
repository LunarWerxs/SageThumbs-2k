//! The WebView2 route: HTML files and .url shortcuts.

use super::*;

/// Whether `path`'s extension is one [`try_load_web`] handles at all (`html`/`htm`/`xhtml`/
/// `url`/`webloc`). Used by `window::on_app_load_resolved` to exclude the `try_load_web` route
/// from its own "apply" stage timing: creating the WebView2 host pumps the message loop for
/// hundreds of ms, which is not a stall, see `window::log_ui_stage_stall`'s doc comment.
pub(in super::super) fn is_web_route_ext(ext: &str) -> bool {
    matches!(ext, "html" | "htm" | "xhtml" | "url" | "webloc")
}

/// Build an HTML/`.url` WebView2 preview when the ext + Settings toggle allow it. Returns true if
/// handled (webview created, a card shown, or the `.url` target shown as text). Falls through
/// (false) to show HTML source as text when the toggle is off or it isn't a web file.
pub(super) unsafe fn try_load_web(hwnd: HWND, path: &str) -> bool {
    let st = &*state(hwnd);
    match ext_of(path).as_str() {
        "html" | "htm" | "xhtml" => {
            if !st2k_base::settings::preview_html() {
                return false; // show source as text instead
            }
            create_web(hwnd, &file_uri(path), super::super::webview::Mode::Local)
        }
        "url" | "webloc" => {
            let Some(target) = parse_url_shortcut(path) else {
                return false;
            };
            if st2k_base::settings::preview_url_live() {
                return create_web(hwnd, &target, super::super::webview::Mode::Live);
            }
            // Text-first (the safe default): show the parsed target; never auto-load.
            *st.text.borrow_mut() = Some(format!(
                "Web shortcut\n\n{target}\n\n(Turn on \"Live .url preview\" in Settings > Quick preview to load it.)"
            ));
            st.kind.set(ContentKind::Text);
            ensure_shown(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            set_title(hwnd);
            super::super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
            true
        }
        _ => false,
    }
}

/// Create the WebView2 host over the content area; on failure show a calm card. Always returns
/// true (the web file is "handled" either way).
///
/// SAFETY-CRITICAL: `webview::create` synchronously PUMPS the message loop while WebView2's async
/// environment/controller initialise, so the wndproc can re-enter during it. We set `busy` first so
/// close/switch requests are DEFERRED (see `request_close`/`request_load`), and after the pump we
/// RE-VALIDATE the window (it may have been destroyed) and re-fetch state before touching it — the
/// `st` from before the pump could be dangling.
pub(super) unsafe fn create_web(hwnd: HWND, url: &str, mode: super::super::webview::Mode) -> bool {
    {
        let st = &*state(hwnd);
        st.kind.set(ContentKind::Html);
        st.busy.set(true);
    }
    ensure_shown(hwnd); // realise the window so the child has a parent + size
    let cr = content_rect(hwnd);
    let host = super::super::webview::create(hwnd, &cr, url, mode); // PUMPS the message loop

    // The pump may have destroyed the window (close-while-loading) — never touch freed state.
    if !windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() {
        return true; // `host` drops here, closing the controller
    }
    let st = &*state(hwnd);
    st.busy.set(false);
    // Apply anything deferred during the pump. A close wins; a newer file-switch means our host is
    // stale (drop it and load the newer path instead of clobbering it).
    if st.pending_close.take() {
        drop(host);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        return true;
    }
    // Take into a `let` FIRST: an `if let` scrutinee's `RefMut` lives for the whole block on
    // edition 2021, and `load` below can re-enter `create_web`, which pumps the message loop —
    // a switch request arriving during THAT pump writes `pending_path` (see `request_load`) and
    // would hit a BorrowMutError, which `panic=abort` turns into a dead viewer.
    let pending = st.pending_path.borrow_mut().take();
    if let Some(p) = pending {
        drop(host);
        load(hwnd, &p);
        return true;
    }
    match host {
        Some(h) => {
            *st.webview.borrow_mut() = Some(h);
            set_title(hwnd);
            super::super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
        }
        None => {
            // Runtime missing / async failed → fall back to a calm card.
            let p = st.path.borrow().clone().unwrap_or_else(|| url.to_string());
            *st.card.borrow_mut() = Some(infocard::gather(&p));
            st.kind.set(ContentKind::InfoCard);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    true
}

/// Turn a local path into a `file:///` URI (forward slashes, minimal escaping of %/space/#/?).
/// `%` goes first and must be escaped at all: `a%2Fb.html` is a legal file name, and WebView2
/// decodes the URI it is handed, so an unescaped one opened `a/b.html`.
pub(super) fn file_uri(path: &str) -> String {
    let esc = path
        .replace('%', "%25")
        .replace('\\', "/")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F");
    if esc.starts_with('/') {
        format!("file://{esc}")
    } else {
        format!("file:///{esc}")
    }
}

/// Decode a `.url`/`.webloc` shortcut's raw bytes as text. Windows commonly writes `.url`
/// files with a non-ASCII target as UTF-16 (LE, with BOM) — a plain `read_to_string`
/// (UTF-8 only) silently failed on those, so the live-preview feature never engaged for
/// them. Sniff the BOM and decode accordingly; UTF-8 (the common case, and `.webloc`'s
/// plist encoding) falls through unchanged.
pub(super) fn decode_shortcut_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (chunks, _) = rest.as_chunks::<2>();
        let units: Vec<u16> = chunks.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16(&units).ok();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (chunks, _) = rest.as_chunks::<2>();
        let units: Vec<u16> = chunks.iter().map(|c| u16::from_be_bytes(*c)).collect();
        return String::from_utf16(&units).ok();
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes); // UTF-8 BOM
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// A real `.url` / `.webloc` is a few hundred bytes; anything past this is not a shortcut, and
/// this runs on the UI thread before any capped read.
const MAX_SHORTCUT_BYTES: u64 = 1 << 20;

/// Parse a `.url`/`.webloc` shortcut for its target. `.url` is an INI (`URL=` under
/// `[InternetShortcut]`); `.webloc` is a plist with a `<string>` URL. `None` unless the scheme is
/// http(s) — so WebView2 never gets a `file:`/`javascript:` target from a shortcut.
pub(super) fn parse_url_shortcut(path: &str) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_SHORTCUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SHORTCUT_BYTES {
        return None;
    }
    let text = decode_shortcut_text(&bytes)?;
    let url = if path.to_ascii_lowercase().ends_with(".webloc") {
        let a = text.find("<string>")? + 8;
        let b = text[a..].find("</string>")? + a;
        text[a..b].trim().to_string()
    } else {
        text.lines()
            .find_map(|l| {
                let t = l.trim();
                t.strip_prefix("URL=").or_else(|| t.strip_prefix("url="))
            })?
            .trim()
            .to_string()
    };
    let low = url.to_ascii_lowercase();
    (low.starts_with("http://") || low.starts_with("https://")).then_some(url)
}
