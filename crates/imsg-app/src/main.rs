//! imsg-app: a portable desktop client for imessaged, talking to
//! imsg-bridge over the tailnet (docs/PORTABLE-WINDOWS.md, docs/BRIDGE.md).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appdir;
mod config;
mod images;
mod logging;
mod notify;
mod secrets;
mod state;
mod ui;

slint::include_modules!();

fn main() {
    let dirs = appdir::AppDirs::detect();
    if let Err(e) = dirs.ensure() {
        fatal(&format!("Cannot create the data folder {}: {e}", dirs.root.display()));
    }
    let (config, config_error) = config::Config::load(&dirs.config_file());
    if config.log
        && let Err(e) = logging::init(&dirs.logs_dir())
    {
        eprintln!("imsg-app: no log file: {e}");
    }
    log::info!("imsg-app {} starting (portable: {})", env!("CARGO_PKG_VERSION"), dirs.portable);
    // Left over from a crash: copies of attachments handed to other apps.
    dirs.wipe_open_dir();

    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("imsg-io").enable_all().build() {
        Ok(r) => r,
        Err(e) => fatal(&format!("Cannot start: {e}")),
    };
    let window = match AppWindow::new() {
        Ok(w) => w,
        Err(e) => fatal(&format!("Cannot open the window: {e}")),
    };
    ui::start(&window, runtime.handle().clone(), dirs.clone(), config, config_error);
    if let Err(e) = window.run() {
        log::error!("event loop: {e}");
    }
    ui::shutdown();
    drop(window);
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    dirs.wipe_open_dir();
    log::info!("imsg-app stopped");
}

fn fatal(msg: &str) -> ! {
    log::error!("{msg}");
    eprintln!("imsg-app: {msg}");
    #[cfg(windows)]
    {
        // No console in release builds: show a message box.
        let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = "imsg".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }
    std::process::exit(1);
}
