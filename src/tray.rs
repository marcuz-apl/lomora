//! The notification-area icon that keeps a windowless Lomora clickable.
//!
//! Windows-only by nature: the Unix `--hidden` path is a daemon with a pid file, and a launch from
//! a shell keeps its console, so neither needs a tray. A tray is started only when this process
//! closed the console it owns (double-click, a shortcut, autostart), because that is the launch
//! where the dashboard would otherwise be reachable only by remembering the address.
//!
//! The menu opens the dashboard in the default browser, toggles the SQL console for this
//! session, shows the product facts, links to the release page, and quits. Quit stops the server
//! through a channel rather than `process::exit`, so in-flight SQLite writes are allowed to
//! finish. If the icon cannot be created at all, startup continues exactly as before.

use crate::config::SqlGate;

/// The text shown when the pointer rests on the icon. Must stay inside `NOTIFYICONDATAW::szTip`,
/// which holds 128 UTF-16 units including the terminating NUL.
pub fn tooltip(port: u16) -> String {
    format!("Lomora — http://localhost:{port}")
}

/// The one-time notification shown when the icon appears, so a user who double-clicked and saw
/// nothing knows where the dashboard went.
pub fn balloon(port: u16) -> String {
    format!("Lomora is running — open http://localhost:{port}")
}

/// The icon as 16x16 BGRA pixels: the accent tile `#1F6FEB` with a white `L`. Drawn in code so the
/// binary needs no icon file and no resource compiler.
pub fn icon_bgra() -> Vec<u8> {
    const SIDE: usize = 16;
    // #1F6FEB, byte order B, G, R, A.
    let accent = [0xEB, 0x6F, 0x1F, 0xFF];
    let white = [0xFF, 0xFF, 0xFF, 0xFF];
    let mut px = vec![0u8; SIDE * SIDE * 4];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let in_upright = (4..=5).contains(&x) && (3..=12).contains(&y);
            let in_foot = (6..=11).contains(&x) && (11..=12).contains(&y);
            let color = if in_upright || in_foot { white } else { accent };
            let i = (y * SIDE + x) * 4;
            px[i..i + 4].copy_from_slice(&color);
        }
    }
    px
}

/// The facts the About item shows, mirroring the dashboard's About dialog. The version is
/// compiled from `Cargo.toml`, so it cannot drift from `/api/version`.
pub fn about_text(version: &str, port: u16) -> String {
    format!(
        "Lomora {version}\n\
         We gauge models\n\n\
         A web dashboard for benchmarking and monitoring local LLM servers — LM Studio, \
         Ollama, Moderado Gateway, or any OpenAI-compatible endpoint.\n\n\
         Dashboard: http://localhost:{port}\n\
         GitHub: https://github.com/marcuz-apl/lomora\n\
         MIT License — © 2026 Lomorat, Alfazen Inc."
    )
}

/// The confirmation shown before the tray opens the SQL console. It must be specific: Lomora
/// listens on every interface, so opening the console exposes row-level SQL to the whole
/// network, and the schema guard — which protects the tables' structure — does not protect
/// their contents.
pub fn sql_warning(port: u16) -> String {
    format!(
        "The SQL console runs row-level SQL against the Lomora database: SELECT, INSERT, UPDATE \
         and DELETE all work, and a script is not undone.\n\n\
         Dropping tables, indexes, views and triggers is refused, as are ALTER TABLE and other \
         schema changes — but rows can still be rewritten or deleted without undo.\n\n\
         Lomora listens on every network interface, so while the console is open, anyone who can \
         reach this computer on port {port} can run the same SQL.\n\n\
         Enable it for this session?"
    )
}

/// Copy a string into a fixed-size UTF-16 field, truncating to leave room for the NUL. The
/// notification data's `szTip` and `szInfo` are raw arrays, and their length is part of the ABI.
pub fn copy_wide<const N: usize>(dst: &mut [u16; N], src: &str) {
    let limit = N.saturating_sub(1);
    let mut written = 0;
    for unit in src.encode_utf16() {
        if written == limit { break; }
        dst[written] = unit;
        written += 1;
    }
    if written < N { dst[written] = 0; }
}

/// Starts the tray when this launch closed the console. The receiver resolves when the user picks
/// Quit; `None` means there is no tray, and the caller must not wait for anything. `sql` is the
/// server's live gate, so the Settings item and the request handlers act on the same state.
pub fn start(hidden: bool, port: u16, sql: SqlGate) -> Option<tokio::sync::oneshot::Receiver<()>> {
    if !hidden { return None; }
    #[cfg(windows)]
    { win::start(port, sql) }
    #[cfg(not(windows))]
    {
        let _ = (port, sql);
        None
    }
}

#[cfg(windows)]
mod win {
    use std::mem::size_of;
    use std::sync::{Mutex, OnceLock};

    use crate::config::SqlGate;
    use windows::core::{BOOL, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, BITMAPINFO,
        BITMAPINFOHEADER, HBRUSH, HGDIOBJ,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
        ShellExecuteW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HCURSOR,
        HICON, ICONINFO, IDI_APPLICATION, IDYES, LoadIconW, MB_DEFBUTTON2, MB_ICONINFORMATION,
        MB_ICONWARNING, MB_OK, MB_YESNO, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
        MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNORMAL,
        SetForegroundWindow, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
        WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WNDCLASS_STYLES,
    };

    const ICON_ID: u32 = 1;
    const OPEN_ID: usize = 1;
    const QUIT_ID: usize = 2;
    const SQL_ID: usize = 3;
    const ABOUT_ID: usize = 4;
    const UPDATES_ID: usize = 5;
    /// Mouse events arrive on this message; `lParam` carries the actual event.
    const TRAY_MESSAGE: u32 = WM_APP + 1;
    const RELEASES_URL: &str = "https://github.com/marcuz-apl/lomora/releases";

    /// The URL is kept as UTF-16 because `ShellExecuteW` reads it after the thread that built the
    /// string is long gone. Quit travels through a channel, never `process::exit`. `sql` is the
    /// server's gate, so the menu cannot report a state the routes do not enforce.
    struct Shared {
        url: Vec<u16>,
        port: u16,
        sql: SqlGate,
        quit: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    /// One tray per process; `start` is called once, from `serve`.
    static SHARED: OnceLock<Shared> = OnceLock::new();

    pub fn start(port: u16, sql: SqlGate) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let url = format!("http://localhost:{port}");
        if SHARED.set(Shared { url: wide(&url), port, sql, quit: Mutex::new(Some(tx)) }).is_err() {
            return None;
        }
        // The message loop needs its own thread: it blocks in `GetMessageW` for the life of the
        // process. The thread owns nothing the server needs, so a failed spawn simply means no
        // tray - the dashboard still serves.
        std::thread::Builder::new()
            .name("lomora-tray".to_string())
            .spawn(move || run(port))
            .ok()
            .map(|_| rx)
    }

    fn run(port: u16) {
        unsafe {
            let Ok(module) = GetModuleHandleW(None) else { return };
            let class = wide("LomoraTray");
            let title = wide("Lomora");
            let icon = make_icon();
            let wc = WNDCLASSW {
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: HINSTANCE(module.0),
                hIcon: icon,
                hCursor: HCURSOR::default(),
                hbrBackground: HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: PCWSTR(class.as_ptr()),
            };
            if RegisterClassW(&wc) == 0 { return; }
            // A zero-size hidden window: it exists only to receive the icon's callbacks.
            let Ok(hwnd) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WINDOW_STYLE(0),
                0, 0, 0, 0,
                None, None, Some(HINSTANCE(module.0)), None,
            ) else { return };

            let nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: ICON_ID,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_INFO,
                uCallbackMessage: TRAY_MESSAGE,
                hIcon: icon,
                szTip: field(&super::tooltip(port)),
                szInfo: field(&super::balloon(port)),
                szInfoTitle: field("Lomora"),
                dwInfoFlags: NIIF_INFO,
                ..Default::default()
            };
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() { return; }

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                TRAY_MESSAGE => {
                    match (lparam.0 & 0xFFFF) as u32 {
                        WM_LBUTTONDBLCLK => open_dashboard(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                        _ => {}
                    }
                    LRESULT(0)
                }
                WM_COMMAND => {
                    match wparam.0 & 0xFFFF {
                        OPEN_ID => open_dashboard(),
                        SQL_ID => toggle_sql(hwnd),
                        ABOUT_ID => show_about(hwnd),
                        UPDATES_ID => open_releases(),
                        QUIT_ID => quit(hwnd),
                        _ => {}
                    }
                    LRESULT(0)
                }
                WM_DESTROY => {
                    remove_icon(hwnd);
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    fn open_dashboard() {
        let Some(shared) = SHARED.get() else { return };
        let operation = wide("open");
        unsafe {
            let _ = ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(shared.url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let Ok(settings) = CreatePopupMenu() else {
                let _ = DestroyMenu(menu);
                return;
            };
            let open = wide("Open dashboard");
            let settings_label = wide("Settings");
            let sql = wide("Allow SQL CRUD (this session)");
            let about = wide("About");
            let updates = wide("Check for updates…");
            let quit = wide("Quit");
            // Built per click, so the checkmark is always the gate the routes are reading.
            let checked = SHARED.get().is_some_and(|shared| shared.sql.enabled());
            let sql_flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(settings, sql_flags, SQL_ID, PCWSTR(sql.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, OPEN_ID, PCWSTR(open.as_ptr()));
            // For `MF_POPUP`, the item id is the submenu's handle; `menu` owns it from here, and
            // destroying the parent destroys the submenu with it.
            let _ = AppendMenuW(menu, MF_POPUP, settings.0 as usize,
                                PCWSTR(settings_label.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, ABOUT_ID, PCWSTR(about.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, UPDATES_ID, PCWSTR(updates.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, QUIT_ID, PCWSTR(quit.as_ptr()));
            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_ok() {
                // Without the foreground window the menu does not dismiss when the user clicks
                // elsewhere; the trailing WM_NULL is the documented companion to that trick.
                let _ = SetForegroundWindow(hwnd);
                let _ = TrackPopupMenu(
                    menu,
                    TPM_LEFTALIGN | TPM_RIGHTBUTTON,
                    point.x, point.y,
                    Some(0),
                    hwnd,
                    None,
                );
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            }
            let _ = DestroyMenu(menu);
        }
    }

    /// Opens or closes the console for this session. Opening asks first and defaults to No: the
    /// gate guards row-level SQL on a listener that reaches the whole LAN. Closing needs no
    /// ceremony — it can only reduce what is reachable.
    fn toggle_sql(hwnd: HWND) {
        let Some(shared) = SHARED.get() else { return };
        if shared.sql.enabled() {
            shared.sql.set(false);
            return;
        }
        let text = wide(&super::sql_warning(shared.port));
        let title = wide("Lomora — SQL console");
        let answer = unsafe {
            MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2)
        };
        if answer.0 == IDYES.0 {
            shared.sql.set(true);
        }
    }

    fn show_about(hwnd: HWND) {
        let Some(shared) = SHARED.get() else { return };
        let text = wide(&super::about_text(env!("CARGO_PKG_VERSION"), shared.port));
        let title = wide("About Lomora");
        unsafe {
            MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()),
                        MB_OK | MB_ICONINFORMATION);
        }
    }

    /// Until a signed updater exists, the honest action is to show the release page rather than
    /// promise an in-app update.
    fn open_releases() {
        let operation = wide("open");
        let url = wide(RELEASES_URL);
        unsafe {
            let _ = ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    /// The server stops when this channel resolves, so the icon goes first: a click between the
    /// request and the exit must not reach a tray whose server is gone.
    fn quit(hwnd: HWND) {
        if let Some(shared) = SHARED.get()
            && let Ok(mut slot) = shared.quit.lock()
            && let Some(tx) = slot.take()
        {
            let _ = tx.send(());
        }
        unsafe { let _ = DestroyWindow(hwnd); }
    }

    fn remove_icon(hwnd: HWND) {
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            ..Default::default()
        };
        unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &nid); }
    }

    /// The tray icon, drawn rather than loaded from a file. A 32-bit top-down DIB supplies the
    /// colour and alpha; an all-zero 1-bit mask marks every pixel opaque, which the alpha channel
    /// then refines.
    unsafe fn make_icon() -> HICON {
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 16,
                    // Negative height: rows run top to bottom, matching `icon_bgra`.
                    biHeight: -16,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let Ok(color) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                return fallback_icon();
            };
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(color.0));
                return fallback_icon();
            }
            let pixels = super::icon_bgra();
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());

            let mask = CreateBitmap(16, 16, 1, 1, None);
            if mask.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(color.0));
                return fallback_icon();
            }
            let icon_info = ICONINFO {
                fIcon: BOOL(1),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: color,
            };
            let icon = CreateIconIndirect(&icon_info);
            let _ = DeleteObject(HGDIOBJ(color.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
            match icon {
                Ok(icon) if !icon.is_invalid() => icon,
                // A tray with the generic application icon is still better than no tray.
                _ => fallback_icon(),
            }
        }
    }

    unsafe fn fallback_icon() -> HICON {
        unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default()
    }

    /// UTF-16 with a terminating NUL, the form every Win32 wide-string parameter expects.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A `NOTIFYICONDATAW` text field, zero-filled first so the unused tail is a defined value.
    fn field<const N: usize>(text: &str) -> [u16; N] {
        let mut buffer = [0u16; N];
        super::copy_wide(&mut buffer, text);
        buffer
    }
}

#[cfg(test)]
mod tests {
    use super::{about_text, balloon, copy_wide, icon_bgra, sql_warning, start, tooltip};
    use crate::config::SqlGate;

    #[test]
    fn the_tooltip_names_the_dashboard_address() {
        assert_eq!(tooltip(8501), "Lomora — http://localhost:8501");
        assert_eq!(tooltip(18503), "Lomora — http://localhost:18503");
    }

    // `szTip` is [u16; 128]; an overlong string would be silently cut by the copy helper, and the
    // address - the only actionable part - would be the part lost.
    #[test]
    fn the_tooltip_fits_the_notification_data_field() {
        assert!(tooltip(u16::MAX).encode_utf16().count() < 128);
    }

    // `szInfo` is [u16; 256]. The balloon is the only hint that the launch succeeded, so it must
    // not be the field that gets truncated.
    #[test]
    fn the_balloon_names_the_address_and_fits_its_field() {
        let text = balloon(8501);
        assert_eq!(text, "Lomora is running — open http://localhost:8501");
        assert!(balloon(u16::MAX).encode_utf16().count() < 256);
    }

    #[test]
    fn the_icon_is_a_white_l_on_the_accent_tile() {
        let px = icon_bgra();
        assert_eq!(px.len(), 16 * 16 * 4);
        let at = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        // BGRA for #1F6FEB.
        let accent = [0xEB, 0x6F, 0x1F, 0xFF];
        let white = [0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(at(0, 0), accent);
        assert_eq!(at(15, 15), accent);
        assert_eq!(at(4, 3), white, "top of the upright");
        assert_eq!(at(5, 12), white, "bottom of the upright");
        assert_eq!(at(11, 12), white, "end of the foot");
        assert_eq!(at(6, 3), accent, "to the right of the upright is still tile");
        assert_eq!(at(12, 12), accent, "one pixel past the foot is tile");
    }

    #[test]
    fn copy_wide_terminates_and_truncates() {
        let mut buf = [7u16; 5];
        copy_wide(&mut buf, "abc");
        assert_eq!(&buf[..4], &['a' as u16, 'b' as u16, 'c' as u16, 0]);
        assert_eq!(buf[4], 7, "units past the terminator are untouched");

        let mut small = [7u16; 4];
        copy_wide(&mut small, "abcdef");
        assert_eq!(&small[..3], &['a' as u16, 'b' as u16, 'c' as u16]);
        assert_eq!(small[3], 0);
    }

    #[test]
    fn the_about_box_carries_the_product_facts() {
        let text = about_text("0.1.7", 8501);
        assert!(text.contains("Lomora 0.1.7"), "{text}");
        assert!(text.contains("We gauge models"), "{text}");
        assert!(text.contains("http://localhost:8501"), "{text}");
        assert!(text.contains("https://github.com/marcuz-apl/lomora"), "{text}");
        assert!(text.contains("MIT License"), "{text}");
    }

    // The warning is the only thing standing between a mis-click and row-level SQL on a 0.0.0.0
    // bind, so it must name the address, the risk, and the guard's limit — it refuses schema
    // changes, not deletes — rather than ask a vague "are you sure?".
    #[test]
    fn the_sql_warning_names_the_port_the_risk_and_the_session_limit() {
        let text = sql_warning(18513);
        assert!(text.contains("port 18513"), "{text}");
        assert!(text.contains("Dropping tables"), "must name what is refused: {text}");
        assert!(text.contains("DELETE"), "must name row-level SQL as allowed: {text}");
        assert!(text.contains("without undo"), "must not imply the guard protects rows: {text}");
        assert!(text.contains("this session"), "{text}");
        assert!(text.contains("network interface"), "{text}");
    }

    #[test]
    fn there_is_no_tray_when_the_console_stays_open() {
        assert!(start(false, 8501, SqlGate::new(true)).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn there_is_no_tray_off_windows() {
        assert!(start(true, 8501, SqlGate::new(false)).is_none());
    }
}
