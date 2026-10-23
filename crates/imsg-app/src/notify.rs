//! Desktop notifications behind a small trait.
//!
//! * Windows: a tray icon (`Shell_NotifyIconW`) whose balloon shows new
//!   messages. No AUMID, no Start-menu shortcut, no registry: nothing is
//!   installed. Clicking the icon or the balloon brings the window back.
//!   (Explorer remembers tray icons it has seen under
//!   `HKCU\…\TrayNotify`; that trace is documented in README-portable.txt.)
//! * Elsewhere (development on Linux): logged only.
//!
//! Taskbar flashing is done by the UI (winit's `request_user_attention`).

pub trait Notifier {
    /// Show a notification (title = sender, body = text or "New message").
    fn notify(&self, title: &str, body: &str);
    /// Tray tooltip, e.g. "imsg – 3 unread".
    fn set_tooltip(&self, _text: &str) {}
    /// Remove the tray icon (at exit).
    fn shutdown(&self) {}
}

/// The platform's notifier; `on_activate` runs (on the tray thread) when
/// the user clicks the icon or a balloon.
pub fn create(on_activate: Box<dyn Fn() + Send + Sync>) -> Box<dyn Notifier> {
    #[cfg(windows)]
    {
        match tray::Tray::start(on_activate) {
            Some(t) => Box::new(t),
            None => Box::new(LogNotifier),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = on_activate;
        Box::new(LogNotifier)
    }
}

pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify(&self, _title: &str, _body: &str) {
        log::info!("notification (not shown on this platform)");
    }
}

#[cfg(windows)]
mod tray {
    use std::collections::VecDeque;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONUSERCLICK,
        NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MSG, PostMessageW,
        PostQuitMessage, RegisterClassW, RegisterWindowMessageW, TranslateMessage, WM_APP, WM_DESTROY, WM_LBUTTONUP, WNDCLASSW,
    };

    use super::Notifier;

    /// NIN_SELECT | NINF_KEY (not exported by windows-sys).
    const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
    const WM_TRAY: u32 = WM_APP + 1;
    const WM_SHOW_PENDING: u32 = WM_APP + 2;
    const WM_QUIT_TRAY: u32 = WM_APP + 3;
    const WM_TIP: u32 = WM_APP + 4;

    struct Shared {
        hwnd: AtomicIsize,
        icon: AtomicIsize,
        taskbar_created: AtomicU32,
        queue: Mutex<VecDeque<(String, String)>>,
        tip: Mutex<String>,
        on_activate: Box<dyn Fn() + Send + Sync>,
    }

    static SHARED: OnceLock<Shared> = OnceLock::new();

    pub struct Tray {
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Copy into a fixed WCHAR buffer, cut to fit, NUL-terminated.
    fn fill(dst: &mut [u16], s: &str) {
        let mut n = 0;
        for u in s.encode_utf16() {
            if n + 1 >= dst.len() {
                break;
            }
            dst[n] = u;
            n += 1;
        }
        dst[n] = 0;
    }

    fn base(hwnd: HWND) -> NOTIFYICONDATAW {
        // SAFETY: plain C struct; all-zero is a valid initial value.
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid
    }

    unsafe fn add_icon(hwnd: HWND) {
        let s = SHARED.get().unwrap();
        let mut nid = base(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = s.icon.load(Ordering::SeqCst) as _;
        fill(&mut nid.szTip, &s.tip.lock().unwrap());
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let s = SHARED.get().unwrap();
        match msg {
            WM_TRAY => {
                let ev = (lparam as u32) & 0xffff;
                if ev == NIN_BALLOONUSERCLICK || ev == WM_LBUTTONUP || ev == NIN_SELECT || ev == NIN_KEYSELECT {
                    (s.on_activate)();
                }
                0
            }
            WM_SHOW_PENDING => {
                let pending: Vec<(String, String)> = s.queue.lock().unwrap().drain(..).collect();
                // Only the newest is worth a balloon; Windows queues them anyway.
                if let Some((title, body)) = pending.last() {
                    let mut nid = base(hwnd);
                    nid.uFlags = NIF_INFO;
                    fill(&mut nid.szInfoTitle, title);
                    fill(&mut nid.szInfo, if body.is_empty() { " " } else { body });
                    nid.dwInfoFlags = NIIF_USER;
                    nid.hBalloonIcon = s.icon.load(Ordering::SeqCst) as _;
                    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
                }
                0
            }
            WM_TIP => {
                let mut nid = base(hwnd);
                nid.uFlags = NIF_TIP;
                fill(&mut nid.szTip, &s.tip.lock().unwrap());
                unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
                0
            }
            WM_QUIT_TRAY => {
                let nid = base(hwnd);
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &nid);
                    DestroyWindow(hwnd);
                }
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            m if m != 0 && m == s.taskbar_created.load(Ordering::SeqCst) => {
                // Explorer restarted: the icon is gone, add it again.
                unsafe { add_icon(hwnd) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    impl Tray {
        pub fn start(on_activate: Box<dyn Fn() + Send + Sync>) -> Option<Tray> {
            let shared = Shared {
                hwnd: AtomicIsize::new(0),
                icon: AtomicIsize::new(0),
                taskbar_created: AtomicU32::new(0),
                queue: Mutex::new(VecDeque::new()),
                tip: Mutex::new("imsg".into()),
                on_activate,
            };
            SHARED.set(shared).ok()?;
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("tray".into())
                .spawn(move || unsafe {
                    let s = SHARED.get().unwrap();
                    let hinst = GetModuleHandleW(null());
                    let class = wide("imsg-app-tray");
                    // SAFETY: zeroed WNDCLASSW, then the fields we need.
                    let mut wc: WNDCLASSW = std::mem::zeroed();
                    wc.lpfnWndProc = Some(wndproc);
                    wc.hInstance = hinst;
                    wc.lpszClassName = class.as_ptr();
                    RegisterClassW(&wc);
                    let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
                    if hwnd.is_null() {
                        let _ = tx.send(false);
                        return;
                    }
                    s.hwnd.store(hwnd as isize, Ordering::SeqCst);
                    s.taskbar_created.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::SeqCst);
                    // Resource id 1 = the app icon (build.rs); else the stock one.
                    let mut icon = LoadIconW(hinst, 1 as _);
                    if icon.is_null() {
                        icon = LoadIconW(null_mut(), IDI_APPLICATION);
                    }
                    s.icon.store(icon as isize, Ordering::SeqCst);
                    add_icon(hwnd);
                    let _ = tx.send(true);
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                })
                .ok()?;
            if rx.recv().unwrap_or(false) {
                Some(Tray { thread: Mutex::new(Some(thread)) })
            } else {
                None
            }
        }

        fn post(msg: u32) {
            if let Some(s) = SHARED.get() {
                let hwnd = s.hwnd.load(Ordering::SeqCst);
                if hwnd != 0 {
                    unsafe { PostMessageW(hwnd as HWND, msg, 0, 0) };
                }
            }
        }
    }

    impl Notifier for Tray {
        fn notify(&self, title: &str, body: &str) {
            if let Some(s) = SHARED.get() {
                s.queue.lock().unwrap().push_back((title.to_string(), body.to_string()));
            }
            Self::post(WM_SHOW_PENDING);
        }

        fn set_tooltip(&self, text: &str) {
            if let Some(s) = SHARED.get() {
                *s.tip.lock().unwrap() = text.to_string();
            }
            Self::post(WM_TIP);
        }

        fn shutdown(&self) {
            Self::post(WM_QUIT_TRAY);
            if let Some(t) = self.thread.lock().unwrap().take() {
                let _ = t.join();
            }
        }
    }
}
