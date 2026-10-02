//! The ticker window: creation, message handling, and the glue between data, rendering and pacing.
//!
//! The window is a frameless, horizontally resizable bar. It is an ordinary top-level window, so
//! PowerToys "Always On Top" (Win+Ctrl+T) pins it like any other.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::fs;
use std::mem::size_of;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize,
};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::anim::{Anim, WM_FRAME};
use crate::config::{self, Config, WindowState};
use crate::icon;
use crate::render::{Content, Renderer, rgb};
use crate::worker::{self, Entry, Shared, WM_QUOTES};

const CLASS_NAME: PCWSTR = w!("StockRockWindow");
/// Defined in commctrl.h; declared here to avoid enabling the whole Controls feature for one value.
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HOUSEKEEPING: usize = 1;
const HOUSEKEEPING_MS: u32 = 1000;

const ID_REFRESH: usize = 1;
const ID_PAUSE: usize = 2;
const ID_EDIT_CONFIG: usize = 3;
const ID_EXIT: usize = 4;

/// Shows a fatal startup error to the user.
pub fn fatal(message: &str) {
    let text = to_wide(message);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("StockRock"),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn run() -> Result<(), String> {
    unsafe {
        // Per-monitor DPI awareness keeps text crisp on mixed-DPI setups.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Only one ticker at a time: a second launch just brings the first one forward.
        let _instance = CreateMutexW(None, false, w!("Local\\StockRock.SingleInstance"))
            .map_err(|e| format!("Could not create the instance mutex: {e}"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(CLASS_NAME, PCWSTR::null()) {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
            }
            return Ok(());
        }

        let (config, config_error) = match config::load() {
            Ok(config) => (config, None),
            Err(error) => (Config::default(), Some(error)),
        };
        let config = Arc::new(config);

        let placement = initial_placement();
        let renderer = Renderer::new(placement.dpi, config.font_size);
        let bar_height = renderer.bar_height();

        let app = Box::new(App::new(
            config.clone(),
            config_error,
            renderer,
            placement.dpi,
            Shared::new(config),
        ));
        // Ownership moves to the window; it is reclaimed in WM_NCDESTROY.
        let app = Box::into_raw(app);

        let instance =
            GetModuleHandleW(None).map_err(|e| format!("GetModuleHandle failed: {e}"))?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err("Could not register the window class.".into());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("StockRock"),
            WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX | WS_SYSMENU,
            placement.x,
            placement.y,
            placement.width,
            bar_height,
            None,
            None,
            Some(instance.into()),
            Some(app as *const c_void),
        )
        .map_err(|e| format!("Could not create the window: {e}"))?;

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            (*(create.lpCreateParams as *const App)).hwnd.set(hwnd);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let app = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
        if app.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(app as *mut App));
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        // `App` uses interior mutability throughout, so nested messages (sent from inside
        // handlers, e.g. by SetWindowPos or a menu's modal loop) only ever see shared references.
        match (*app).handle(msg, wparam, lparam) {
            Some(result) => result,
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

struct Note {
    text: String,
    error: bool,
}

struct App {
    hwnd: Cell<HWND>,
    shared: Arc<Shared>,
    anim: OnceCell<Arc<Anim>>,
    config: RefCell<Arc<Config>>,
    config_error: RefCell<Option<String>>,
    config_stamp: Cell<Option<SystemTime>>,
    config_pending: Cell<Option<SystemTime>>,
    render: RefCell<Renderer>,
    entries: RefCell<Vec<Entry>>,
    /// Status shown in place of quotes until real data arrives.
    note: RefCell<Option<Note>>,
    /// Client size in physical pixels.
    view: Cell<(i32, i32)>,
    dpi: Cell<u32>,
    /// Scroll position in pixels.
    offset: Cell<f64>,
    last_frame: Cell<Instant>,
    hovered: Cell<bool>,
    tracking: Cell<bool>,
    paused: Cell<bool>,
    minimized: Cell<bool>,
    cloaked: Cell<bool>,
    trimmed: Cell<bool>,
}

impl App {
    fn new(
        config: Arc<Config>,
        config_error: Option<String>,
        renderer: Renderer,
        dpi: u32,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            hwnd: Cell::new(HWND::default()),
            shared,
            anim: OnceCell::new(),
            config: RefCell::new(config),
            config_error: RefCell::new(config_error),
            config_stamp: Cell::new(config_modified()),
            config_pending: Cell::new(None),
            render: RefCell::new(renderer),
            entries: RefCell::new(Vec::new()),
            note: RefCell::new(Some(Note {
                text: "Loading quotes\u{2026}".into(),
                error: false,
            })),
            view: Cell::new((0, 0)),
            dpi: Cell::new(dpi),
            offset: Cell::new(0.0),
            last_frame: Cell::new(Instant::now()),
            hovered: Cell::new(false),
            tracking: Cell::new(false),
            paused: Cell::new(false),
            minimized: Cell::new(false),
            cloaked: Cell::new(false),
            trimmed: Cell::new(false),
        }
    }

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let hwnd = self.hwnd.get();
        unsafe {
            match msg {
                WM_CREATE => {
                    self.on_create();
                    Some(LRESULT(0))
                }
                // No non-client area at all: the whole window is our canvas.
                WM_NCCALCSIZE if wparam.0 != 0 => Some(LRESULT(0)),
                WM_NCHITTEST => Some(LRESULT(self.hit_test(lparam) as isize)),
                WM_GETMINMAXINFO => {
                    self.limit_size(lparam);
                    Some(LRESULT(0))
                }
                WM_SIZE => {
                    let minimized = wparam.0 as u32 == SIZE_MINIMIZED;
                    self.minimized.set(minimized);
                    if !minimized {
                        self.view.set((loword(lparam.0), hiword(lparam.0)));
                        self.rebuild();
                    }
                    self.sync_animation();
                    Some(LRESULT(0))
                }
                WM_DPICHANGED => {
                    self.dpi.set((wparam.0 & 0xFFFF) as u32);
                    let suggested = &*(lparam.0 as *const RECT);
                    self.apply_metrics(Some(suggested));
                    Some(LRESULT(0))
                }
                WM_ERASEBKGND => Some(LRESULT(1)),
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut paint);
                    self.draw(dc);
                    let _ = EndPaint(hwnd, &paint);
                    Some(LRESULT(0))
                }
                WM_FRAME => {
                    self.on_frame();
                    Some(LRESULT(0))
                }
                WM_QUOTES => {
                    self.on_quotes();
                    Some(LRESULT(0))
                }
                WM_TIMER if wparam.0 == TIMER_HOUSEKEEPING => {
                    self.housekeeping();
                    Some(LRESULT(0))
                }
                // Drag the window from anywhere on the bar.
                WM_LBUTTONDOWN => {
                    let _ = ReleaseCapture();
                    SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        Some(WPARAM(HTCAPTION as usize)),
                        Some(LPARAM(0)),
                    );
                    Some(LRESULT(0))
                }
                WM_MOUSEMOVE => {
                    if !self.tracking.get() {
                        let mut track = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        self.tracking.set(TrackMouseEvent(&mut track).is_ok());
                    }
                    if !self.hovered.replace(true) {
                        self.sync_animation();
                    }
                    Some(LRESULT(0))
                }
                WM_MOUSELEAVE => {
                    self.tracking.set(false);
                    if self.hovered.replace(false) {
                        self.sync_animation();
                    }
                    Some(LRESULT(0))
                }
                WM_CONTEXTMENU => {
                    let (x, y) = if lparam.0 == -1 {
                        // Invoked from the keyboard: anchor to the window's top-left.
                        let mut rect = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rect);
                        (rect.left, rect.bottom)
                    } else {
                        (loword(lparam.0), hiword(lparam.0))
                    };
                    self.show_menu(x, y);
                    Some(LRESULT(0))
                }
                WM_EXITSIZEMOVE => {
                    self.save_placement();
                    None
                }
                WM_DESTROY => {
                    self.save_placement();
                    let _ = KillTimer(Some(hwnd), TIMER_HOUSEKEEPING);
                    self.shared.quit();
                    if let Some(anim) = self.anim.get() {
                        anim.quit();
                    }
                    PostQuitMessage(0);
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }
    }

    fn on_create(&self) {
        let hwnd = self.hwnd.get();
        unsafe {
            let corners = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corners as *const _ as *const c_void,
                size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
            let border = rgb(0x2A, 0x2E, 0x36);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const _ as *const c_void,
                size_of::<COLORREF>() as u32,
            );
            SetTimer(Some(hwnd), TIMER_HOUSEKEEPING, HOUSEKEEPING_MS, None);

            // Taskbar and Alt-Tab icons, sized for this monitor's DPI.
            for (kind, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
                if let Some(icon) = icon::create(GetSystemMetricsForDpi(metric, self.dpi.get())) {
                    SendMessageW(
                        hwnd,
                        WM_SETICON,
                        Some(WPARAM(kind as usize)),
                        Some(LPARAM(icon.0 as isize)),
                    );
                }
            }
        }

        let config = self.config.borrow().clone();
        let _ = self.anim.set(Anim::start(hwnd, config.fps));
        worker::spawn(self.shared.clone(), hwnd);
    }

    /// Only the left and right edges resize; the height is fixed by the font size.
    fn hit_test(&self, lparam: LPARAM) -> u32 {
        let x = loword(lparam.0);
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd.get(), &mut rect);
        }
        let edge = (6 * self.dpi.get() as i32 / 96).max(4);
        if x < rect.left + edge {
            HTLEFT
        } else if x >= rect.right - edge {
            HTRIGHT
        } else {
            HTCLIENT
        }
    }

    fn limit_size(&self, lparam: LPARAM) {
        let height = self.render.borrow().bar_height();
        let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
        info.ptMinTrackSize = POINT {
            x: 240 * self.dpi.get() as i32 / 96,
            y: height,
        };
        info.ptMaxTrackSize.y = height;
    }

    /// Applies DPI/font changes: new fonts, new bar height, and (when moving between monitors)
    /// the position and width Windows suggests.
    fn apply_metrics(&self, suggested: Option<&RECT>) {
        let hwnd = self.hwnd.get();
        let font_size = self.config.borrow().font_size;
        self.render
            .borrow_mut()
            .set_metrics(self.dpi.get(), font_size);
        let height = self.render.borrow().bar_height();

        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut rect);
            if let Some(suggested) = suggested {
                rect = *suggested;
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        // The size may not have changed (so no WM_SIZE), but the fonts did.
        self.rebuild();
    }

    /// Re-lays-out the tape for the current data, size and fonts, then shows it.
    fn rebuild(&self) {
        let (width, height) = self.view.get();
        {
            let entries = self.entries.borrow();
            let note = self.note.borrow();
            let config_error = self.config_error.borrow();
            let message;
            let content = if let Some(error) = config_error.as_deref() {
                message = format!("Config error \u{2014} {error} \u{2014} right-click to edit");
                Content::Message {
                    text: &message,
                    error: true,
                }
            } else if let Some(note) = note.as_ref() {
                Content::Message {
                    text: &note.text,
                    error: note.error,
                }
            } else {
                Content::Quotes(&entries)
            };
            self.render.borrow_mut().build(content, width, height);
        }
        let period = self.render.borrow().period();
        self.offset.set(if period > 0 {
            self.offset.get() % period as f64
        } else {
            0.0
        });
        self.sync_animation();
        self.present();
    }

    fn draw(&self, dc: HDC) {
        let (width, height) = self.view.get();
        self.render
            .borrow()
            .blit(dc, width, height, self.offset.get() as i32);
    }

    /// Draws straight to the window, bypassing WM_PAINT (which is low priority and can lag).
    fn present(&self) {
        let hwnd = self.hwnd.get();
        unsafe {
            let dc = GetDC(Some(hwnd));
            if !dc.is_invalid() {
                self.draw(dc);
                ReleaseDC(Some(hwnd), dc);
            }
        }
    }

    fn on_frame(&self) {
        if let Some(anim) = self.anim.get() {
            anim.frame_done();
        }
        if !self.want_animation() {
            return;
        }
        let now = Instant::now();
        // Time-based motion: speed stays constant even if a frame is late.
        let dt = now
            .saturating_duration_since(self.last_frame.replace(now))
            .as_secs_f64()
            .min(0.1);
        let period = self.render.borrow().period();
        if period > 0 {
            let speed = self.config.borrow().scroll_speed * self.dpi.get() as f64 / 96.0;
            self.offset
                .set((self.offset.get() + speed * dt) % period as f64);
        }
        self.present();
    }

    fn want_animation(&self) -> bool {
        let config = self.config.borrow();
        self.render.borrow().scrolls()
            && config.scroll_speed > 0.0
            && !self.paused.get()
            && !(self.hovered.get() && config.pause_on_hover)
            && !self.minimized.get()
            && !self.cloaked.get()
    }

    fn sync_animation(&self) {
        let Some(anim) = self.anim.get() else { return };
        let want = self.want_animation();
        if want && !anim.is_active() {
            self.last_frame.set(Instant::now());
        }
        anim.set_active(want);
    }

    fn on_quotes(&self) {
        let Some(update) = self.shared.take_update() else {
            return;
        };
        let any_quote = update.entries.iter().any(|e| e.quote.is_some());
        *self.note.borrow_mut() = if any_quote {
            None
        } else {
            Some(match &update.error {
                Some(error) => Note {
                    text: format!("Can't load quotes \u{2014} {error}"),
                    error: true,
                },
                None => Note {
                    text: "Loading quotes\u{2026}".into(),
                    error: false,
                },
            })
        };
        *self.entries.borrow_mut() = update.entries;
        self.set_title(update.error.as_deref());
        self.rebuild();

        if !self.trimmed.replace(true) {
            // Startup touched a lot of memory that is never needed again; hand it back.
            unsafe {
                let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
            }
        }
    }

    /// The window title is only visible in the taskbar/Alt-Tab, which makes it a good place for
    /// status without cluttering the ticker itself.
    fn set_title(&self, error: Option<&str>) {
        let now = unsafe { GetLocalTime() };
        let at = format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond);
        let title = match error {
            None => format!("StockRock \u{2014} updated {at}"),
            Some(error) => format!("StockRock \u{2014} {error} (as of {at})"),
        };
        let title = to_wide(&title);
        unsafe {
            let _ = SetWindowTextW(self.hwnd.get(), PCWSTR(title.as_ptr()));
        }
    }

    /// Runs once a second: watches for virtual-desktop switches and config file edits.
    fn housekeeping(&self) {
        let mut cloaked = 0u32;
        let queried = unsafe {
            DwmGetWindowAttribute(
                self.hwnd.get(),
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut c_void,
                size_of::<u32>() as u32,
            )
        };
        let cloaked = queried.is_ok() && cloaked != 0;
        if cloaked != self.cloaked.replace(cloaked) {
            self.sync_animation();
        }

        // Reload once the modification time has been stable for a tick: editors often write the
        // file in two steps and we don't want to parse it half-written.
        let stamp = config_modified();
        if stamp != self.config_stamp.get() {
            if stamp == self.config_pending.get() {
                self.config_stamp.set(stamp);
                self.config_pending.set(None);
                self.reload_config();
            } else {
                self.config_pending.set(stamp);
            }
        }
    }

    fn reload_config(&self) {
        match config::load() {
            Ok(new) => {
                let new = Arc::new(new);
                *self.config_error.borrow_mut() = None;
                *self.config.borrow_mut() = new.clone();
                if let Some(anim) = self.anim.get() {
                    anim.set_fps(new.fps);
                }
                self.shared.set_config(new);
                self.apply_metrics(None);
            }
            Err(error) => {
                *self.config_error.borrow_mut() = Some(error);
                self.rebuild();
            }
        }
    }

    fn show_menu(&self, x: i32, y: i32) {
        let hwnd = self.hwnd.get();
        let command = unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let pause = if self.paused.get() {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(menu, MF_STRING, ID_REFRESH, w!("Refresh now"));
            let _ = AppendMenuW(menu, pause, ID_PAUSE, w!("Pause scrolling"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_EDIT_CONFIG, w!("Edit config\u{2026}"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));
            // Required so the menu dismisses when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                x,
                y,
                None,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            command.0 as usize
        };

        match command {
            ID_REFRESH => self.shared.refresh(),
            ID_PAUSE => {
                self.paused.set(!self.paused.get());
                self.sync_animation();
            }
            ID_EDIT_CONFIG => {
                let path = to_wide(&format!("\"{}\"", config::config_path().display()));
                unsafe {
                    ShellExecuteW(
                        Some(hwnd),
                        w!("open"),
                        w!("notepad.exe"),
                        PCWSTR(path.as_ptr()),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            ID_EXIT => unsafe {
                // Posted (not sent) so the window is destroyed after this handler has returned.
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            _ => {}
        }
    }

    fn save_placement(&self) {
        let hwnd = self.hwnd.get();
        unsafe {
            if IsIconic(hwnd).as_bool() {
                return;
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                let dpi = self.dpi.get().max(1) as i32;
                config::save_state(&WindowState {
                    x: rect.left,
                    y: rect.top,
                    width_dip: (rect.right - rect.left) * 96 / dpi,
                });
            }
        }
    }
}

struct Placement {
    x: i32,
    y: i32,
    width: i32,
    dpi: u32,
}

/// Restores the saved position if it is still on a connected monitor; otherwise centers the bar
/// along the top of the primary monitor's work area.
fn initial_placement() -> Placement {
    unsafe {
        if let Some(state) = config::load_state() {
            let probe = POINT {
                x: state.x + 24,
                y: state.y + 12,
            };
            let monitor = MonitorFromPoint(probe, MONITOR_DEFAULTTONULL);
            if !monitor.is_invalid() {
                let dpi = monitor_dpi(monitor);
                let work = work_area(monitor);
                let width = (state.width_dip * dpi as i32 / 96).min(work.right - work.left);
                return Placement {
                    x: state.x,
                    y: state.y,
                    width,
                    dpi,
                };
            }
        }

        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let dpi = monitor_dpi(monitor);
        let work = work_area(monitor);
        let margin = 8 * dpi as i32 / 96;
        let width = (1000 * dpi as i32 / 96).min(work.right - work.left - 2 * margin);
        Placement {
            x: work.left + (work.right - work.left - width) / 2,
            y: work.top + margin,
            width,
            dpi,
        }
    }
}

unsafe fn monitor_dpi(monitor: HMONITOR) -> u32 {
    let (mut x, mut y) = (0u32, 0u32);
    match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => x,
        _ => 96,
    }
}

unsafe fn work_area(monitor: HMONITOR) -> RECT {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        }
    }
}

fn config_modified() -> Option<SystemTime> {
    fs::metadata(config::config_path())
        .and_then(|m| m.modified())
        .ok()
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn loword(value: isize) -> i32 {
    (value & 0xFFFF) as i16 as i32
}

fn hiword(value: isize) -> i32 {
    ((value >> 16) & 0xFFFF) as i16 as i32
}
