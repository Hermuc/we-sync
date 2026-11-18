#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "ui")]
use std::sync::mpsc;

#[cfg(feature = "ui")]
use we_sync::config::AppConfig;
#[cfg(feature = "ui")]
use we_sync::engine;
#[cfg(feature = "ui")]
use we_sync::ui;
#[cfg(feature = "ui")]
use we_sync::util;

fn main() {
    #[cfg(feature = "ui")]
    run_gui();
    #[cfg(not(feature = "ui"))]
    {
        eprintln!("we-sync: GUI 需要 ui feature（默认启用；库使用者可关闭）");
        std::process::exit(1);
    }
}

#[cfg(feature = "ui")]
fn run_gui() {
    // ---- 单实例：已存在则唤起已有窗口
    if !acquire_single_instance() {
        util::log(util::Level::Info, "检测到已在运行，唤起已有实例后退出");
        wake_existing();
        return;
    }

    util::log(
        util::Level::Info,
        format!(
            "WE-Sync {} 启动，目录: {}",
            we_sync::VERSION,
            util::app_dir().display()
        ),
    );

    let cfg = AppConfig::load();
    let (evt_tx, evt_rx) = mpsc::channel();
    let handle = engine::spawn(cfg.clone(), evt_tx);

    let code = ui::run(cfg, handle, evt_rx);
    util::log(util::Level::Info, format!("程序退出，代码 {code}"));
}

fn acquire_single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        let handle = CreateMutexW(None, true, w!("WE-Sync-Single-Instance-Mutex"));
        if handle.is_err() {
            return true; // 拿不到互斥体就不阻止启动
        }
        let err = GetLastError();
        // 句柄故意泄漏，进程结束自动回收
        std::mem::forget(handle);
        err != ERROR_ALREADY_EXISTS
    }
}

fn wake_existing() {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_SHOW,
    };
    unsafe {
        let cls = HSTRING::from("WESyncMainWindow");
        if let Ok(hwnd) = FindWindowW(&cls, None) {
            if !hwnd.is_invalid() {
                if !IsWindowVisible(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }
}
