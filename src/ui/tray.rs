//! 托盘图标：Shell_NotifyIcon + 右键菜单。

use windows::core::w;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, SetForegroundWindow,
    TrackPopupMenu, IDI_APPLICATION, MENU_ITEM_FLAGS, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON,
};

pub const WM_TRAY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
pub const TRAY_UID: u32 = 1;

pub const CMD_SHOW: usize = 2001;
pub const CMD_SYNC: usize = 2002;
pub const CMD_PAUSE: usize = 2003;
pub const CMD_OPEN_LOG: usize = 2004;
pub const CMD_QUIT: usize = 2005;

pub struct Tray {
    hwnd: HWND,
    added: bool,
    tooltip: String,
    icon: HICON,
}

impl Tray {
    pub fn new(hwnd: HWND, icon: HICON) -> Self {
        let mut t = Self {
            hwnd,
            added: false,
            tooltip: "WE-Sync".into(),
            icon,
        };
        t.add();
        t
    }

    fn data(&self, tip: &str) -> NOTIFYICONDATAW {
        let mut d = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: if self.icon.is_invalid() {
                unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }
            } else {
                self.icon
            },
            ..Default::default()
        };
        let mut i = 0;
        for c in tip.encode_utf16().take(127) {
            d.szTip[i] = c;
            i += 1;
        }
        d.szTip[i] = 0;
        d
    }

    fn add(&mut self) {
        let d = self.data(&self.tooltip);
        self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &d).as_bool() };
    }

    pub fn update_tip(&mut self, tip: &str) {
        if self.tooltip == tip {
            return;
        }
        let short: String = tip.chars().take(120).collect();
        self.tooltip = short;
        let d = self.data(&self.tooltip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    pub fn show_menu(&self, paused: bool) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let _ = AppendMenuW(menu, MF_STRING, CMD_SHOW, w!("显示设置"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_SYNC, w!("立即同步"));
            let flags = MF_STRING | if paused { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
            let _ = AppendMenuW(menu, flags, CMD_PAUSE, w!("暂停监视"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, w!(""));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_LOG, w!("打开日志"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("退出"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // 前台化，确保点击别处时菜单能关掉
            let _ = SetForegroundWindow(self.hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            let d = self.data(&self.tooltip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &d);
            }
            self.added = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}
