//! 设置窗口（原生 Win32，无外部 UI 依赖，便于单文件绿色发布）。

pub mod icon;
pub mod theme;
pub mod tray;

use std::sync::mpsc::Receiver;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, MapWindowPoints, ScreenToClient, SetBkColor, SetBkMode,
    SetTextColor, UpdateWindow, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC,
    PAINTSTRUCT, GetWindowDC, ReleaseDC,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SCROLLCARET, EM_SETSEL, MEASUREITEMSTRUCT, ODT_COMBOBOX, ODT_LISTBOX,
    ODS_DISABLED, ODS_SELECTED, GetComboBoxInfo, COMBOBOXINFO,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, IsWindowEnabled, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::AppConfig;
use crate::discover;
use crate::engine::{self, Cmd, EngineHandle, Event, Stats};
use crate::transport::DeviceStatus;
use crate::util;

use tray::{
    CMD_OPEN_LOG, CMD_PAUSE, CMD_QUIT, CMD_SHOW, CMD_SYNC, Tray, WM_TRAY,
};

const CLASS_NAME: &str = "WESyncMainWindow";
const TITLE: &str = "WE-Sync · Wallpaper Engine 壁纸同步";

// 控件 ID
const ID_WORKSHOP: i32 = 1004;
const ID_BROWSE: i32 = 1005;
const ID_REMOTE: i32 = 1001;
const ID_MIN: i32 = 1002;
const ID_MAX: i32 = 1003;
const ID_CHANNEL: i32 = 1006;
const ID_ADB: i32 = 1007;
const ID_TRAY_CHK: i32 = 1008;
const ID_MP4_CHK: i32 = 1009;
const ID_SAVE: i32 = 1100;
const ID_SYNC_NOW: i32 = 1101;
const ID_OPEN_WS: i32 = 1102;
const ID_OPEN_DATA: i32 = 1103;
const ID_STATUS: i32 = 1200;
const ID_PROGRESS: i32 = 1201;
const ID_STATS: i32 = 1202;
const ID_LOG: i32 = 1203;
const ID_SAVE_HINT: i32 = 1204;
const ID_PAUSE_TRANSFER: i32 = 1104;
const CHANNEL_LABELS: [&str; 4] = [
    "自动（按速度择优）",
    "仅 MTP",
    "仅 ADB",
    "多通道（USB+WiFi）",
];

const TIMER_ID: usize = 1;
/// WM_MOUSELEAVE（windows crate 未导出该常量）
const MSG_MOUSE_LEAVE: u32 = 0x02A3;

pub fn run(cfg: AppConfig, engine: EngineHandle, evt_rx: Receiver<Event>) -> i32 {
    unsafe {
        let _ = SetProcessDPIAware();
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();

        let cls_name = HSTRING::from(CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: PCWSTR(cls_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return 1;
        }

        let tray_chk = cfg.general.close_to_tray;
        let mp4_chk = cfg.source.mp4_only;
        let ui = Box::new(Ui {
            cfg,
            engine,
            evt_rx,
            handles: Default::default(),
            theme: None,
            hicon: HICON::default(),
            hicon_small: HICON::default(),
            cap_icon: HICON::default(),
            hot_btn: 0,
            device: None,
            busy: false,
            progress: String::from("空闲"),
            stats: Stats::default(),
            queue: 0,
            last_log_text: String::new(),
            paused: false,
            transfer_paused: false,
            tray_chk,
            mp4_chk,
            watcher: None,
            tray: None,
            log_dirty: false,
        });
        let raw = Box::into_raw(ui);

        // 按系统 DPI 放大初始窗口，否则高分屏下会显得过小
        let sys_dpi = GetDpiForSystem();
        let sf = if sys_dpi == 0 { 1.0 } else { sys_dpi as f32 / 96.0 };
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &HSTRING::from(CLASS_NAME),
            &HSTRING::from(TITLE),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (820.0 * sf) as i32,
            (730.0 * sf) as i32,
            HWND::default(),
            HMENU::default(),
            hinst,
            Some(raw as *const _),
        ) {
            Ok(h) => h,
            Err(e) => {
                drop(Box::from_raw(raw));
                crate::log_error!("创建窗口失败: {e}");
                return 1;
            }
        };

        let _ = ShowWindow(hwnd, SW_SHOW);
        Ui::style_frame(hwnd);
        let _ = UpdateWindow(hwnd);
        let _ = SetTimer(hwnd, TIMER_ID, 250, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        msg.wParam.0 as i32
    }
}

struct Ui {
    cfg: AppConfig,
    engine: EngineHandle,
    evt_rx: Receiver<Event>,
    handles: std::collections::HashMap<i32, HWND>,
    theme: Option<theme::Theme>,
    /// 32×32 应用图标（窗口大图标）
    hicon: HICON,
    /// 16×16 小图标（托盘）
    hicon_small: HICON,
    /// 标题栏专用的全透明图标（占位但不显示图形）
    cap_icon: HICON,
    /// 鼠标悬停的按钮 ID（0 = 无），用于自绘按钮的高亮
    hot_btn: i32,
    device: Option<DeviceStatus>,
    busy: bool,
    progress: String,
    stats: Stats,
    queue: usize,
    last_log_text: String,
    paused: bool,
    transfer_paused: bool,
    /// 两个开关的自绘状态（BS_OWNERDRAW 不再自动维护 check）
    tray_chk: bool,
    mp4_chk: bool,
    watcher: Option<notify::RecommendedWatcher>,
    tray: Option<Tray>,
    log_dirty: bool,
}

impl Ui {
    fn th(&self) -> &theme::Theme {
        self.theme.as_ref().expect("theme 尚未初始化")
    }

    /// 让标题栏、边框与窗口配色统一（Win11 原生属性，失败时静默忽略）。
    unsafe fn style_frame(hwnd: HWND) {
        let caption = theme::CAPTION;
        let text = theme::TEXT;
        let border = theme::BORDER;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const _ as *const core::ffi::c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR,
            &text as *const _ as *const core::ffi::c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as *const core::ffi::c_void,
            4,
        );
    }

    fn h(&self, id: i32) -> HWND {
        self.handles.get(&id).copied().unwrap_or_default()
    }

    fn set_text(&self, id: i32, text: &str) {
        let h = self.h(id);
        if !h.is_invalid() {
            unsafe {
                let s = HSTRING::from(text);
                let _ = SetWindowTextW(h, &s);
            }
        }
    }

    fn get_text(&self, id: i32) -> String {
        let h = self.h(id);
        if h.is_invalid() {
            return String::new();
        }
        unsafe {
            let len = GetWindowTextLengthW(h);
            let mut buf = vec![0u16; len as usize + 2];
            let n = GetWindowTextW(h, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    /// 只在状态真的变化时才调 EnableWindow —— refresh_status 每 250ms 跑一次。
    fn set_enabled(&self, id: i32, on: bool) {
        let h = self.h(id);
        if h.is_invalid() {
            return;
        }
        unsafe {
            if IsWindowEnabled(h).as_bool() != on {
                let _ = EnableWindow(h, on);
            }
        }
    }

    fn refresh_status(&mut self) {
        let dev = match &self.device {
            Some(d) => format!(
                "已连接 [{}] {}   存储: {}",
                d.channel.label(),
                d.name,
                if d.storages.is_empty() {
                    "-".to_string()
                } else {
                    d.storages.join(" / ")
                }
            ),
            None => "等待手机连接（USB 选择「文件传输 / MTP」，或开启 USB 调试走 ADB）".to_string(),
        };
        self.set_text(ID_STATUS, &dev);

        // 两个传输控制按钮互斥：同一时刻只允许一个动作可点。
        // 只改绘制不改 EnableWindow 的话，按钮看起来灰了但点下去仍会触发命令。
        let connected = self.device.is_some();
        let has_work = self.busy || self.queue > 0;
        let (sync_on, pause_on) =
            transfer_button_states(connected, has_work, self.transfer_paused);
        self.set_enabled(ID_SYNC_NOW, sync_on);
        self.set_enabled(ID_PAUSE_TRANSFER, pause_on);

        let prog = if self.transfer_paused {
            format!("已暂停传输 · 待处理 {} 个（点「继续」恢复）", self.queue)
        } else if self.busy {
            format!("传输中: {}", self.progress)
        } else if self.queue > 0 {
            format!("排队中: {} 个待传输", self.queue)
        } else {
            "空闲".to_string()
        };
        self.set_text(ID_PROGRESS, &prog);

        let s = self.stats;
        let q = self.queue;
        self.set_text(
            ID_STATS,
            &format!(
                "本次已复制 {} · 手机上已有 {} · 失败 {} · 体积不达标 {} · 待处理 {}",
                s.uploaded, s.skipped, s.failed, s.filtered, q
            ),
        );

        let tip = if let Some(d) = &self.device {
            format!(
                "WE-Sync · {} · 已复制 {} 失败 {}",
                d.channel.label(),
                s.uploaded,
                s.failed
            )
        } else {
            "WE-Sync · 等待手机".to_string()
        };
        if let Some(t) = self.tray.as_mut() {
            t.update_tip(&tip);
        }
    }

    fn flush_logs(&mut self) {
        if !self.log_dirty {
            return;
        }
        self.log_dirty = false;
        // 单一数据源：直接镜像内存环形日志，避免与事件流重复
        let text = util::recent_logs(300).join("\r\n");
        if text == self.last_log_text {
            return;
        }
        self.last_log_text = text.clone();
        let h = self.h(ID_LOG);
        if !h.is_invalid() {
            unsafe {
                let s = HSTRING::from(text);
                let _ = SetWindowTextW(h, &s);
                // 选中末尾并滚到底（0x7FFF_FFFF 才是 EDIT 认识的"文末"）
                SendMessageW(h, EM_SETSEL, WPARAM(0x7FFF_FFFF), LPARAM(0x7FFF_FFFF));
                SendMessageW(h, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// 从控件读取配置（不落盘）。
    fn collect_config(&self) -> (AppConfig, Vec<String>) {
        let mut cfg = self.cfg.clone();
        let mut errs = Vec::new();

        cfg.source.workshop_dir = self.get_text(ID_WORKSHOP).trim().to_string();
        cfg.target.remote_dir = self.get_text(ID_REMOTE).trim().to_string();
        if cfg.target.remote_dir.is_empty() {
            errs.push("手机目录不能为空".to_string());
        }

        match self.get_text(ID_MIN).trim().parse::<f64>() {
            Ok(v) if v >= 0.0 => cfg.filter.min_mb = v,
            _ => errs.push("最小体积必须是 >= 0 的数字".to_string()),
        }
        match self.get_text(ID_MAX).trim().parse::<f64>() {
            Ok(v) if v >= 0.0 => cfg.filter.max_mb = v,
            _ => errs.push("最大体积必须是 >= 0 的数字".to_string()),
        }
        if cfg.filter.max_mb > 0.0 && cfg.filter.max_mb < cfg.filter.min_mb {
            errs.push("最大体积不能小于最小体积".to_string());
        }
        cfg.transport.adb_path = self.get_text(ID_ADB).trim().to_string();

        // 下拉框仍用原生 CB_GETCURSEL 读选中项（只换了外观，交互未变）
        let sel = unsafe {
            SendMessageW(self.h(ID_CHANNEL), CB_GETCURSEL, WPARAM(0), LPARAM(0)).0
        };
        cfg.transport.prefer = match sel {
            1 => "mtp",
            2 => "adb",
            3 => "multipath",
            _ => "auto",
        }
        .to_string();

        cfg.general.close_to_tray = self.tray_chk;
        cfg.source.mp4_only = self.mp4_chk;

        (cfg, errs)
    }

    fn hwnd_main(&self) -> HWND {
        self.handles.get(&0).copied().unwrap_or_default()
    }

    fn apply_and_sync(&mut self) {
        let (cfg, errs) = self.collect_config();
        if !errs.is_empty() {
            self.set_text(ID_SAVE_HINT, &format!("⚠ {}", errs.join("；")));
            return;
        }
        cfg.save();
        self.cfg = cfg.clone();
        // 只保存；引擎收到新配置后会自行按新条件重扫（暂停状态保持不变）
        let _ = self.engine.tx.send(Cmd::SetConfig(Box::new(cfg.clone())));
        self.set_text(ID_SAVE_HINT, "✓ 设置已保存");
        self.rebuild_watcher();
    }

    fn rebuild_watcher(&mut self) {
        self.watcher = None;
        if let Some(dir) = discover::effective_workshop_dir(&self.cfg) {
            self.watcher = engine::spawn_watcher(dir, self.engine.dirty.clone());
        }
    }

    fn open_path(path: &str) {
        unsafe {
            let op = HSTRING::from("open");
            let p = HSTRING::from(path);
            ShellExecuteW(
                None,
                &op,
                &p,
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }

    fn set_transfer_paused(&mut self, v: bool) {
        self.transfer_paused = v;
        self.set_text(ID_PAUSE_TRANSFER, if v { "继续" } else { "暂停" });
        // 自绘按钮文字变了要主动重绘
        let h = self.h(ID_PAUSE_TRANSFER);
        if !h.is_invalid() {
            unsafe {
                let _ = InvalidateRect(h, None, false);
            }
        }
        // 暂停状态已在状态行说明，这里不再重复长文案
        self.set_text(ID_SAVE_HINT, if v { "已暂停" } else { "" });
    }

    fn on_command(&mut self, id: i32) {
        match id {
            ID_SAVE => self.apply_and_sync(),
            ID_SYNC_NOW => {
                // 暂停中先恢复，否则"立即同步"看起来没反应
                if self.transfer_paused {
                    self.set_transfer_paused(false);
                }
                let _ = self.engine.tx.send(Cmd::SyncNow);
                self.set_text(ID_SAVE_HINT, "已请求立即同步");
            }
            ID_PAUSE_TRANSFER => {
                let next = !self.transfer_paused;
                self.set_transfer_paused(next);
                // 直接置位中止标志：worker 此刻正卡在传输里，收不到命令也要能停
                self.engine
                    .abort
                    .store(next, std::sync::atomic::Ordering::Relaxed);
                let _ = self.engine.tx.send(Cmd::SetTransferPaused(next));
            }
            ID_TRAY_CHK | ID_MP4_CHK => {
                if id == ID_TRAY_CHK {
                    self.tray_chk = !self.tray_chk;
                } else {
                    self.mp4_chk = !self.mp4_chk;
                }
                let h = self.h(id);
                if !h.is_invalid() {
                    unsafe {
                        let _ = InvalidateRect(h, None, false);
                    }
                }
            }
            ID_BROWSE => {
                if let Some(dir) = discover::detect_workshop_dir() {
                    self.set_text(ID_WORKSHOP, &dir.display().to_string());
                    self.set_text(ID_SAVE_HINT, "已自动填入探测到的工坊目录（记住点保存）");
                } else {
                    self.set_text(ID_SAVE_HINT, "未探测到工坊目录，请手动填写");
                }
            }
            ID_OPEN_WS => {
                if let Some(dir) = discover::effective_workshop_dir(&self.cfg) {
                    Self::open_path(&dir.display().to_string());
                } else {
                    self.set_text(ID_SAVE_HINT, "未找到工坊目录");
                }
            }
            ID_OPEN_DATA => Self::open_path(&util::app_dir().display().to_string()),
            x if x as usize == CMD_SHOW => unsafe {
                let _ = ShowWindow(self.hwnd_main(), SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd_main());
            },
            x if x as usize == CMD_SYNC => {
                let _ = self.engine.tx.send(Cmd::SyncNow);
            }
            x if x as usize == CMD_PAUSE => {
                self.paused = !self.paused;
                let _ = self.engine.tx.send(Cmd::SetPaused(self.paused));
            }
            x if x as usize == CMD_OPEN_LOG => {
                let p = util::log_path();
                Self::open_path(&p.display().to_string());
            }
            x if x as usize == CMD_QUIT => unsafe {
                let _ = DestroyWindow(self.hwnd_main());
            },
            _ => {}
        }
    }

    fn on_timer(&mut self, hwnd: HWND) {
        let was_connected = self.device.is_some();
        while let Ok(ev) = self.evt_rx.try_recv() {
            match ev {
                Event::Log(_) => self.log_dirty = true,
                Event::Device(d) => {
                    self.device = d;
                }
                Event::Busy(b) => self.busy = b,
                Event::Progress { name, done, total } => {
                    let pct = if total > 0 {
                        (done as f64 / total as f64 * 100.0) as u32
                    } else {
                        0
                    };
                    self.progress = format!(
                        "{name}  {pct}%  ({}/{})",
                        util::human_size(done),
                        util::human_size(total)
                    );
                }
                Event::TransferPaused(p) => {
                    self.transfer_paused = p;
                    self.set_text(ID_PAUSE_TRANSFER, if p { "继续" } else { "暂停" });
                }
                Event::Stats(_) | Event::Queue(_) => {} // 以共享计数器为准
            }
        }
        // 直接读共享状态：即使 worker 正卡在传输里，数字也保持最新
        let stats_arc = self.engine.stats.clone();
        if let Ok(s) = stats_arc.lock() {
            self.stats = *s;
        }
        self.queue = self
            .engine
            .queue
            .load(std::sync::atomic::Ordering::Relaxed);
        self.refresh_status();
        self.flush_logs();

        // 指示灯是父窗口自绘的，STATIC 文字更新只重绘那个子窗口，父窗口不会跟着重绘——
        // 不显式失效的话，设备断开后文字已变「等待手机连接」而灯还亮着（旧像素残留）。
        if self.device.is_some() != was_connected {
            unsafe {
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
    }
}

fn is_own_button(id: i32) -> bool {
    matches!(
        id,
        ID_SAVE | ID_SYNC_NOW | ID_PAUSE_TRANSFER | ID_OPEN_WS | ID_OPEN_DATA | ID_BROWSE
            | ID_CHANNEL | ID_TRAY_CHK | ID_MP4_CHK
    )
}

/// 「立即同步 / 暂停」的可用性：同一时刻只允许一个动作可点。
///
/// 空闲时没有东西可暂停，只有「立即同步」有意义；一旦有传输或已暂停，
/// 就反过来只留「暂停/继续」。这样两个按钮不会同时亮着，也不会出现
/// "暂停中还能点立即同步"这种自相矛盾的状态。
fn transfer_button_states(connected: bool, has_work: bool, paused: bool) -> (bool, bool) {
    if !connected {
        return (false, false);
    }
    if paused || has_work {
        (false, true)
    } else {
        (true, false)
    }
}

/// 位于白卡片内的自绘控件：圆角之外应露出白卡，而不是暖白窗口底。
/// 漏掉任何一个，圆角外就会留下一圈明显的灰环。
fn is_card_button(id: i32) -> bool {
    matches!(id, ID_TRAY_CHK | ID_MP4_CHK | ID_BROWSE)
}

/// 控件在父窗口客户区里的矩形。
unsafe fn ctl_rect(parent: HWND, ctl: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    if GetWindowRect(ctl, &mut r).is_err() {
        return None;
    }
    let mut pts = [
        POINT { x: r.left, y: r.top },
        POINT { x: r.right, y: r.bottom },
    ];
    MapWindowPoints(HWND::default(), parent, &mut pts);
    Some(RECT {
        left: pts[0].x,
        top: pts[0].y,
        right: pts[1].x,
        bottom: pts[1].y,
    })
}

/// 单行输入框的「视觉框」与控件自身矩形。
unsafe fn edit_visual(hwnd: HWND, h: HWND, t: &theme::Theme) -> Option<(RECT, RECT)> {
    let r = ctl_rect(hwnd, h)?;
    let v = theme::inflate(&r, 1, t.px(7.0), 1, 1);
    Some((v, r))
}

/// 单行输入框视觉框的高度：EDIT 自身高度 + 上扩 7px + 下扩 1px（见 `edit_visual`）。
fn field_frame_height(t: &theme::Theme) -> i32 {
    t.px(22.0) + t.px(7.0) + 1
}

/// 让原生下拉框的「收起态」高度等于 `target`。
///
/// COMBOBOX 的静态区高度 = `CB_SETITEMHEIGHT(-1)` + 固定边框量（125% 缩放下实测 6px），
/// 不能像 EDIT 那样直接把窗口高度设成目标值。这里先用一个探测值把边框量量出来再反推，
/// 避免把边框量当魔法数写死。
fn fit_combo_field(combo: HWND, target: i32) {
    unsafe {
        let probe = 100;
        SendMessageW(
            combo,
            CB_SETITEMHEIGHT,
            WPARAM(usize::MAX),
            LPARAM(probe as isize),
        );
        let mut r = RECT::default();
        let _ = GetWindowRect(combo, &mut r);
        let border = ((r.bottom - r.top) - probe).max(0);
        let item = (target - border).max(1);
        SendMessageW(
            combo,
            CB_SETITEMHEIGHT,
            WPARAM(usize::MAX),
            LPARAM(item as isize),
        );
    }
}

/// 三张白卡片的几何。布局与绘制共用，避免坐标不同步。
/// 顶部品牌区已移除，卡片整体上移，首卡从 24px 起排。
fn card_rects(t: &theme::Theme, cw: i32, ch: i32) -> [RECT; 3] {
    let m = t.px(22.0);
    let mk = |top: f32, bottom: f32| RECT {
        left: m,
        top: t.px(top),
        right: cw - m,
        bottom: t.px(bottom),
    };
    let mut log = mk(390.0, 0.0);
    log.bottom = ch - t.px(18.0);
    [mk(24.0, 218.0), mk(280.0, 376.0), log]
}

/// 父窗口自绘：暖白底 + 白色圆角卡片 + 输入框描边。
unsafe fn paint(ui: &Ui, hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let t = ui.th();

    theme::fill(hdc, &rc, t.br_bg);
    let cards = card_rects(t, rc.right, rc.bottom);

    // 卡片：白色填充 + 1px 极浅描边（PACE 靠留白分层，不用重阴影）
    for c in cards {
        theme::round_rect(hdc, &c, t.px(18.0), theme::SURFACE, None);
        let outer = theme::outset(&c, 1);
        theme::round_stroke(hdc, &outer, t.px(18.0), theme::BORDER);
    }

    // 状态卡片左侧的连接指示灯（蓝=已连接并亮起，灰=等待）
    theme::led(
        hdc,
        cards[1].left + t.px(23.0),
        cards[1].top + t.px(25.0),
        t.px(4.0),
        theme::ACCENT,
        ui.device.is_some(),
    );

    // 单行输入框：父窗口补出完整视觉框（浅底 + 圆角描边），文字因此落在框中心
    for id in [ID_WORKSHOP, ID_REMOTE, ID_MIN, ID_MAX, ID_ADB] {
        if let Some(h) = ui.handles.get(&id).copied() {
            if h.is_invalid() { continue; }
            if let Some((v, _)) = edit_visual(hwnd, h, t) {
                theme::round_rect(hdc, &v, t.px(6.0), theme::FIELD, None);
                theme::round_stroke(hdc, &v, t.px(6.0), theme::BORDER);
            }
        }
    }
    // 多行日志框：只描边
    for id in [ID_LOG] {
        if let Some(h) = ui.handles.get(&id).copied() {
            if h.is_invalid() { continue; }
            if let Some(r) = ctl_rect(hwnd, h) {
                let outer = theme::outset(&r, 1);
                theme::round_stroke(hdc, &outer, t.px(6.0), theme::BORDER);
            }
        }
    }

    let _ = EndPaint(hwnd, &ps);
}

/// 下拉列表项：白底 / 选中浅蓝 + 左侧宝蓝竖条 + 黑色文字。
unsafe fn draw_combo_item(ui: &Ui, dis: &DRAWITEMSTRUCT) {
    let t = ui.th();
    let rc = dis.rcItem;
    let selected = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    theme::fill(
        dis.hDC,
        &rc,
        if selected { t.br_accent_soft } else { t.br_surface },
    );
    if selected {
        let bar = RECT {
            left: rc.left + t.px(8.0),
            top: rc.top + t.px(7.0),
            right: rc.left + t.px(11.0),
            bottom: rc.bottom - t.px(7.0),
        };
        theme::round_rect(dis.hDC, &bar, t.px(2.0), theme::ACCENT, None);
    }
    let mut tr = rc;
    tr.left += t.px(20.0);
    tr.right -= t.px(8.0);
    let text = CHANNEL_LABELS
        .get(dis.itemID as usize)
        .copied()
        .unwrap_or("");
    theme::text(
        dis.hDC,
        text,
        &tr,
        t.font,
        theme::TEXT,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

/// PACE 风格开关：宝蓝/浅灰胶囊轨道 + 白色圆钮 + 黑色文字。
unsafe fn draw_toggle(ui: &Ui, dis: &DRAWITEMSTRUCT) {
    let t = ui.th();
    let id = dis.CtlID as i32;
    let on = if id == ID_TRAY_CHK { ui.tray_chk } else { ui.mp4_chk };
    let hot = ui.hot_btn == id;
    let rc = dis.rcItem;

    theme::fill(dis.hDC, &rc, t.br_surface);

    let tw = t.px(34.0);
    let th = t.px(20.0);
    let cy = (rc.top + rc.bottom) / 2;
    let track = RECT {
        left: rc.left + t.px(2.0),
        top: cy - th / 2,
        right: rc.left + t.px(2.0) + tw,
        bottom: cy + th / 2,
    };
    let brush = if on {
        if hot { theme::ACCENT_HOVER } else { theme::ACCENT }
    } else {
        theme::TRACK_OFF
    };
    theme::round_rect(dis.hDC, &track, th / 2, brush, None);

    // 白色圆钮（无描边，避免出现黑圈）
    let d = th - t.px(6.0);
    let kx = if on {
        track.right - d - t.px(3.0)
    } else {
        track.left + t.px(3.0)
    };
    let knob = RECT {
        left: kx,
        top: cy - d / 2,
        right: kx + d,
        bottom: cy + d / 2,
    };
    theme::ellipse(dis.hDC, &knob, theme::SURFACE);

    let mut tr = rc;
    tr.left = track.right + t.px(10.0);
    theme::text(
        dis.hDC,
        &ui.get_text(id),
        &tr,
        t.font,
        theme::TEXT,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

/// 自绘按钮：实心强调主按钮 + 浅底次按钮，圆角。
unsafe fn draw_button(ui: &Ui, dis: &DRAWITEMSTRUCT) {
    let t = ui.th();
    let id = dis.CtlID as i32;
    let primary = id == ID_SAVE;
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let disabled = (dis.itemState.0 & ODS_DISABLED.0) != 0;
    let hot = ui.hot_btn == id && !disabled;

    // 先清底：否则系统用 WM_CTLCOLORBTN 的刷子填过整个矩形，圆角外会留一圈白边。
    // 卡片内控件清成白（露出白卡），卡片外清成窗口暖白。
    let bg = if is_card_button(id) { t.br_surface } else { t.br_bg };
    theme::fill(dis.hDC, &dis.rcItem, bg);

    let color = if primary {
        if pressed {
            theme::ACCENT_PRESS
        } else if hot {
            theme::ACCENT_HOVER
        } else {
            theme::ACCENT
        }
    } else if disabled {
        theme::BTN_BG_DISABLED
    } else if pressed {
        theme::BTN_BG_PRESS
    } else if hot {
        theme::BTN_BG_HOVER
    } else {
        theme::BTN_BG
    };

    let mut rc = dis.rcItem;
    if primary {
        theme::round_rect(dis.hDC, &rc, t.px(17.0), color, None);
    } else {
        let border = if disabled {
            theme::BORDER_DISABLED
        } else {
            theme::BORDER
        };
        theme::round_rect(dis.hDC, &rc, t.px(17.0), color, Some(border));
        rc.left += 1;
        rc.right -= 1;
    }

    let text = ui.get_text(id);
    let fg = if primary {
        // 宝蓝主按钮用白字，保证对比度；次按钮仍是白/浅底深字。
        if disabled { theme::TEXT_ON_ACCENT_DIM } else { theme::TEXT_ON_ACCENT }
    } else if disabled {
        theme::TEXT_DISABLED
    } else {
        theme::TEXT
    };
    if pressed {
        rc.top += t.px(1.0);
    }
    theme::text(
        dis.hDC,
        &text,
        &rc,
        t.font_bold,
        fg,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

/// 原生下拉框换肤：只接管绘制消息，键盘/鼠标/列表弹出逻辑仍全部由系统处理。
/// 原窗口过程存在 combo 的 GWLP_USERDATA，Ui 指针从父窗口取。
unsafe extern "system" fn combo_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let old: WNDPROC = std::mem::transmute(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as isize);
    if old.is_none() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        // 只返回 0 会留下系统 3D 边框/焦点框的四个黑角：必须主动把整个
        // 窗口矩形（含非客户区）擦成白卡底色，再交给 WM_PAINT 画圆角面。
        WM_NCPAINT => {
            let parent = GetParent(hwnd).unwrap_or_default();
            let ui = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut Ui;
            if !ui.is_null() {
                let dc = GetWindowDC(hwnd);
                if !dc.is_invalid() {
                    let mut wr = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut wr);
                    let whole = RECT {
                        left: 0,
                        top: 0,
                        right: wr.right - wr.left,
                        bottom: wr.bottom - wr.top,
                    };
                    theme::fill(dc, &whole, (*ui).th().br_surface);
                    let _ = ReleaseDC(hwnd, dc);
                }
            }
            return LRESULT(0);
        }
        // 系统会在获得焦点时画黑色虚线焦点框，重绘一次把它盖掉
        WM_SETFOCUS | WM_KILLFOCUS => {
            let r = CallWindowProcW(old, hwnd, msg, wp, lp);
            let _ = InvalidateRect(hwnd, None, false);
            return r;
        }
        WM_PAINT => {
            let parent = GetParent(hwnd).unwrap_or_default();
            let ui = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut Ui;
            if ui.is_null() {
                return CallWindowProcW(old, hwnd, msg, wp, lp);
            }
            let t = (*ui).th();
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);

            // `BeginPaint` 会先用 COMBOBOX 的类背景刷（COLOR_BTNFACE 灰）擦掉整个
            // 更新区域，之后只画圆角面的话，四个圆角外侧就留着灰方块。这里先整块
            // 铺白卡底，再叠圆角面，角上露出的就是卡片白，与父窗口连续。
            theme::fill(hdc, &rc, t.br_surface);

            let hot = (*ui).hot_btn == ID_CHANNEL;
            let bg = if hot { theme::BTN_BG_HOVER } else { theme::FIELD };
            theme::round_rect(hdc, &rc, t.px(6.0), bg, None);
            let focus = GetFocus() == hwnd;
            theme::round_stroke(
                hdc,
                &rc,
                t.px(6.0),
                if focus { theme::ACCENT } else { theme::BORDER },
            );

            let sel = SendMessageW(hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
            let label = CHANNEL_LABELS[sel.clamp(0, CHANNEL_LABELS.len() as isize - 1) as usize];
            let mut tr = rc;
            tr.left += t.px(10.0);
            tr.right -= t.px(26.0);
            theme::text(
                hdc,
                label,
                &tr,
                t.font,
                theme::TEXT,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );

            // 自绘下拉箭头（小 V 折线），替代系统的 3D 灰按钮
            let cx = rc.right - t.px(16.0);
            let cy = (rc.top + rc.bottom) / 2;
            let s = t.px(3.0);
            let pts = [
                POINT { x: cx - s, y: cy - s / 2 },
                POINT { x: cx, y: cy + s / 2 },
                POINT { x: cx + s, y: cy - s / 2 },
            ];
            theme::polyline(hdc, &pts, theme::TEXT, t.px(2.0).max(1));

            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    CallWindowProcW(old, hwnd, msg, wp, lp)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;

    match msg {
        WM_CREATE => {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            let ui = &mut *(cs.lpCreateParams as *mut Ui);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ui as *mut Ui as isize);
            ui.handles.insert(0, hwnd);
            let dpi = {
                let d = GetDpiForWindow(hwnd);
                if d == 0 { 96 } else { d }
            };
            ui.theme = Some(theme::Theme::new(dpi));
            Ui::style_frame(hwnd);
            ui.hicon = icon::make_icon(false).unwrap_or_default();
            ui.hicon_small = icon::make_icon(true).unwrap_or_default();
            if !ui.hicon.is_invalid() {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    WPARAM(ICON_BIG as usize),
                    LPARAM(ui.hicon.0 as isize),
                );
            }
            // 标题栏左侧不放图标：`WM_SETICON(ICON_SMALL, 0)` 会回落到 exe 内嵌图标，
            // 只有塞一张 alpha 全 0 的小图标才能真正不画东西（大图标仍给任务栏/Alt-Tab 用）。
            let cap = (16.0 * dpi as f32 / 96.0).round() as i32;
            ui.cap_icon = icon::blank(cap.max(16)).unwrap_or_default();
            if !ui.cap_icon.is_invalid() {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    WPARAM(ICON_SMALL as usize),
                    LPARAM(ui.cap_icon.0 as isize),
                );
            }
            build_controls(hwnd, ui);
            let tray_icon = if ui.hicon_small.is_invalid() {
                ui.hicon
            } else {
                ui.hicon_small
            };
            ui.tray = Some(Tray::new(hwnd, tray_icon));
            ui.rebuild_watcher();
            ui.refresh_status();
            ui.log_dirty = true;
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // 背景全部在 WM_PAINT 里画，避免闪烁
            LRESULT(1)
        }
        WM_PAINT => {
            if !ptr.is_null() {
                paint(&*ptr, hwnd);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            let ctl = HWND(lp.0 as *mut core::ffi::c_void);
            let id = GetWindowLongPtrW(ctl, GWLP_ID) as i32;
            unsafe {
                SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
                let color = match id {
                    ID_SAVE_HINT => theme::TEXT_DIM,
                    ID_STATUS | ID_PROGRESS => theme::TEXT,
                    ID_STATS => theme::TEXT_DIM,
                    _ if (3000..3100).contains(&id) => theme::TEXT_DIM, // 标签
                    _ => theme::TEXT,
                };
                SetTextColor(hdc, color);
            }
            if ptr.is_null() {
                return LRESULT(0);
            }
            // 卡片外的元素（提示文字、自绘按钮圆角之外）用窗口底色，
            // 否则会露出一块白补丁 / 一圈白边
            let brush = if id == ID_SAVE_HINT
                || (msg == WM_CTLCOLORBTN && is_own_button(id) && !is_card_button(id))
            {
                (*ptr).th().br_bg
            } else {
                (*ptr).th().br_surface
            };
            LRESULT(brush.0 as isize)
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            let is_list = msg == WM_CTLCOLORLISTBOX;
            unsafe {
                SetBkColor(hdc, if is_list { theme::WHITE } else { theme::FIELD });
                SetTextColor(hdc, theme::TEXT);
            }
            if ptr.is_null() {
                return LRESULT(0);
            }
            // 展开的列表用白底（与白卡一致），编辑框仍用浅灰输入框底
            let brush = if is_list {
                (*ptr).th().br_surface
            } else {
                (*ptr).th().br_field
            };
            LRESULT(brush.0 as isize)
        }
        WM_MEASUREITEM => {
            if !ptr.is_null() {
                let mis = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
                if mis.CtlType == ODT_COMBOBOX || mis.CtlType == ODT_LISTBOX {
                    mis.itemHeight = (*ptr).th().px(30.0) as u32;
                    mis.itemWidth = 0;
                    return LRESULT(1);
                }
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            if !ptr.is_null() {
                let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
                let id = dis.CtlID as i32;
                if dis.CtlType == ODT_COMBOBOX || dis.CtlType == ODT_LISTBOX {
                    // 收起时的静态区（itemID = u32::MAX）由 combo_proc 的 WM_PAINT 画
                    if dis.itemID != u32::MAX {
                        draw_combo_item(&*ptr, dis);
                    }
                } else if id == ID_TRAY_CHK || id == ID_MP4_CHK {
                    draw_toggle(&*ptr, dis);
                } else {
                    draw_button(&*ptr, dis);
                }
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            if !ptr.is_null() {
                let ui = &mut *ptr;
                // 离开按钮时收回高亮
                let mut te = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut te);

                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let child = ChildWindowFromPointEx(
                    hwnd,
                    pt,
                    CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
                );
                let id = if child.is_invalid() {
                    0
                } else {
                    GetWindowLongPtrW(child, GWLP_ID) as i32
                };
                let id = if is_own_button(id) { id } else { 0 };
                if id != ui.hot_btn {
                    let old = ui.hot_btn;
                    ui.hot_btn = id;
                    for b in [old, id] {
                        if is_own_button(b) {
                            let h = ui.h(b);
                            if !h.is_invalid() {
                                let _ = InvalidateRect(h, None, false);
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        MSG_MOUSE_LEAVE => {
            if !ptr.is_null() {
                let ui = &mut *ptr;
                if ui.hot_btn != 0 {
                    let old = ui.hot_btn;
                    ui.hot_btn = 0;
                    let h = ui.h(old);
                    if !h.is_invalid() {
                        let _ = InvalidateRect(h, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if !ptr.is_null() {
                let ui = &*ptr;
                let x = (lp.0 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
                for id in [ID_WORKSHOP, ID_REMOTE, ID_MIN, ID_MAX, ID_ADB] {
                    let h = ui.h(id);
                    if h.is_invalid() { continue; }
                    if let Some((v, r)) = edit_visual(hwnd, h, ui.th()) {
                        let in_v = x >= v.left && x <= v.right && y >= v.top && y <= v.bottom;
                        let in_c = x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
                        if in_v && !in_c {
                            let _ = SetFocus(h);
                            break;
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            if !ptr.is_null() {
                (*ptr).on_command(id);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if !ptr.is_null() {
                (*ptr).on_timer(hwnd);
            }
            LRESULT(0)
        }
        WM_TRAY => {
            let ev = lp.0 as u32;
            if !ptr.is_null() {
                let ui = &mut *ptr;
                match ev {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => unsafe {
                        let _ = ShowWindow(hwnd, SW_SHOW);
                        let _ = SetForegroundWindow(hwnd);
                    },
                    WM_RBUTTONUP => {
                        if let Some(t) = ui.tray.as_ref() {
                            t.show_menu(ui.paused);
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if !ptr.is_null() {
                let mut rc = RECT::default();
                if GetClientRect(hwnd, &mut rc).is_ok() {
                    layout_controls(&mut *ptr, rc.right - rc.left, rc.bottom - rc.top);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            let ui = &mut *ptr;
            if ui.cfg.general.close_to_tray {
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            } else {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
        }
        WM_DESTROY => {
            if !ptr.is_null() {
                let _ = (*ptr).engine.tx.send(Cmd::Quit);
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            let _ = KillTimer(hwnd, TIMER_ID);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn build_controls(hwnd: HWND, ui: &mut Ui) {
    unsafe {
        let font = ui.th().font;

        let mk = |class: PCWSTR, text: &str, style: WINDOW_STYLE, id: i32| -> HWND {
            let t = HSTRING::from(text);
            let h = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                &t,
                style,
                0,
                0,
                10,
                10,
                hwnd,
                HMENU(id as isize as *mut core::ffi::c_void),
                HINSTANCE::default(),
                None,
            )
            .unwrap_or_default();
            if !h.is_invalid() {
                SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            }
            h
        };

        let label = |text: &str, id: i32| mk(w!("STATIC"), text, WS_CHILD | WS_VISIBLE, id);

        ui.handles.insert(
            ID_WORKSHOP,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                ID_WORKSHOP,
            ),
        );
        ui.handles.insert(
            ID_BROWSE,
            mk(
                w!("BUTTON"),
                "自动探测",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_BROWSE,
            ),
        );
        ui.handles.insert(
            ID_REMOTE,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                ID_REMOTE,
            ),
        );
        ui.handles.insert(
            ID_MIN,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32),
                ID_MIN,
            ),
        );
        ui.handles.insert(
            ID_MAX,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32),
                ID_MAX,
            ),
        );
        ui.handles.insert(
            ID_CHANNEL,
            mk(
                w!("COMBOBOX"),
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE(
                        CBS_DROPDOWNLIST as u32
                            | CBS_OWNERDRAWFIXED as u32
                            | CBS_HASSTRINGS as u32
                            | WS_VSCROLL.0,
                    ),
                ID_CHANNEL,
            ),
        );
        ui.handles.insert(
            ID_ADB,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                ID_ADB,
            ),
        );
        ui.handles.insert(
            ID_TRAY_CHK,
            mk(
                w!("BUTTON"),
                "关闭窗口时最小化到托盘",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_TRAY_CHK,
            ),
        );
        ui.handles.insert(
            ID_MP4_CHK,
            mk(
                w!("BUTTON"),
                "只复制 mp4 视频壁纸",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_MP4_CHK,
            ),
        );
        ui.handles.insert(
            ID_SAVE,
            mk(
                w!("BUTTON"),
                "保存设置",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_SAVE,
            ),
        );
        ui.handles.insert(
            ID_SYNC_NOW,
            mk(
                w!("BUTTON"),
                "立即同步",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_SYNC_NOW,
            ),
        );
        ui.handles.insert(
            ID_PAUSE_TRANSFER,
            mk(
                w!("BUTTON"),
                "暂停",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_PAUSE_TRANSFER,
            ),
        );
        ui.handles.insert(
            ID_OPEN_WS,
            mk(
                w!("BUTTON"),
                "打开工坊目录",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_OPEN_WS,
            ),
        );
        ui.handles.insert(
            ID_OPEN_DATA,
            mk(
                w!("BUTTON"),
                "打开软件目录",
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                ID_OPEN_DATA,
            ),
        );
        ui.handles.insert(
            ID_STATUS,
            mk(
                w!("STATIC"),
                "",
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0000_8000u32),
                ID_STATUS,
            ),
        );
        ui.handles.insert(
            ID_PROGRESS,
            mk(
                w!("STATIC"),
                "",
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0000_8000u32),
                ID_PROGRESS,
            ),
        );
        ui.handles.insert(
            ID_STATS,
            mk(
                w!("STATIC"),
                "",
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0000_8000u32),
                ID_STATS,
            ),
        );
        ui.handles.insert(
            ID_SAVE_HINT,
            mk(w!("STATIC"), "", WS_CHILD | WS_VISIBLE, ID_SAVE_HINT),
        );
        ui.handles.insert(
            ID_LOG,
            mk(
                w!("EDIT"),
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | WINDOW_STYLE(ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32),
                ID_LOG,
            ),
        );

        // 输入框去掉系统边框后需要手动内边距，否则文字贴边
        for (id, f) in [
            (ID_STATUS, ui.th().font_bold),
            (ID_LOG, ui.th().font_small),
        ] {
            let hh = ui.h(id);
            if !hh.is_invalid() {
                SendMessageW(hh, WM_SETFONT, WPARAM(f.0 as usize), LPARAM(1));
            }
        }
        for id in [ID_WORKSHOP, ID_REMOTE, ID_MIN, ID_MAX, ID_ADB, ID_LOG] {
            let hh = ui.h(id);
            if !hh.is_invalid() {
                let m = (6 * ui.th().dpi as i32) / 96;
                let pad = m | (m << 16);
                SendMessageW(hh, 0x00D3 /* EM_SETMARGINS */, WPARAM(3), LPARAM(pad as isize));
            }
        }

        // 静态标签（不需要句柄保存）
        for (id, text) in (3000..).zip(["工坊目录", "手机目录", "体积过滤", "传输通道", "日志"]) {
            let hh = label(text, id);
            ui.handles.insert(id, hh);
        }
        for (lid, text) in [
            (3100, "MB ~"),
            (3101, "MB（0 表示不限）"),
            (3102, "ADB 路径"),
        ] {
            let hh = label(text, lid);
            ui.handles.insert(lid, hh);
        }

        // 初始值
        let ws = discover::effective_workshop_dir(&ui.cfg)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ui.cfg.source.workshop_dir.clone());
        ui.set_text(ID_WORKSHOP, &ws);
        ui.set_text(ID_REMOTE, &ui.cfg.target.remote_dir);
        ui.set_text(ID_MIN, &fmt_num(ui.cfg.filter.min_mb));
        ui.set_text(ID_MAX, &fmt_num(ui.cfg.filter.max_mb));
        ui.set_text(ID_ADB, &ui.cfg.transport.adb_path);

        // 通道下拉
        let combo = ui.h(ID_CHANNEL);
        // 收起态高度必须与右侧输入框的视觉框等高（否则一高一矮很扎眼）。
        // COMBOBOX 的静态区高度 = `CB_SETITEMHEIGHT(-1)` + 固定的边框量，不能像 EDIT
        // 那样直接给窗口高度，所以先量出边框量再反推项目高；列表项仍用 30px 保持行距。
        fit_combo_field(combo, field_frame_height(ui.th()));
        SendMessageW(combo, CB_SETITEMHEIGHT, WPARAM(0), LPARAM(ui.th().px(30.0) as isize));
        for item in CHANNEL_LABELS {
            let h = HSTRING::from(item);
            SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(h.as_ptr() as isize));
        }
        let sel = match ui.cfg.transport.prefer.as_str() {
            "adb" => 2,
            "mtp" => 1,
            "multipath" => 3,
            _ => 0,
        };
        SendMessageW(combo, CB_SETCURSEL, WPARAM(sel), LPARAM(0));

        // 换肤：只接管绘制消息，保留原生下拉/键盘逻辑
        let old = SetWindowLongPtrW(combo, GWLP_WNDPROC, combo_proc as *const () as isize);
        SetWindowLongPtrW(combo, GWLP_USERDATA, old);
        let _ = InvalidateRect(combo, None, false);

        // 展开列表（ComboLBox）自带的 1px 黑框与白卡风格不搭，去掉非客户边框
        let mut cbi = COMBOBOXINFO {
            cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        if GetComboBoxInfo(combo, &mut cbi).is_ok() && !cbi.hwndList.is_invalid() {
            let list = cbi.hwndList;
            let st = GetWindowLongPtrW(list, GWL_STYLE) & !(WS_BORDER.0 as isize);
            SetWindowLongPtrW(list, GWL_STYLE, st);
            let ex = GetWindowLongPtrW(list, GWL_EXSTYLE)
                & !(WS_EX_CLIENTEDGE.0 as isize)
                & !(WS_EX_STATICEDGE.0 as isize);
            SetWindowLongPtrW(list, GWL_EXSTYLE, ex);
            let _ = SetWindowPos(
                list,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }

        // 两个开关的状态由 Ui 维护（构造时已从 cfg 读入），无需 BM_SETCHECK

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        layout_controls(ui, rc.right - rc.left, rc.bottom - rc.top);
    }
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < f64::EPSILON {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn layout_controls(ui: &mut Ui, cw: i32, ch: i32) {
    unsafe {
        let t = ui.th();
        let px = |v: f32| t.px(v);
        let cards = card_rects(t, cw, ch);
        let cfg = cards[0];
        let log = cards[2];

        let pad = px(18.0);
        let label_w = px(76.0);
        let field_x = cfg.left + pad + label_w + px(8.0);
        let field_w = cfg.right - pad - field_x;
        let hi = px(28.0);
        let eh = px(22.0);      // 单行 EDIT 实际高度
        let eoff = px(6.0);     // 下移量：让文字落在视觉框中心
        let gap = px(34.0);
        let ly = |row_y: i32| row_y + px(4.0);

        let mv = |id: i32, x: i32, y: i32, w: i32, h: i32| {
            let hh = ui.h(id);
            if !hh.is_invalid() {
                let _ = MoveWindow(hh, x, y, w, h, true);
            }
        };

        // ---- 卡片 1：配置表单 ----
        let r1 = cfg.top + px(18.0);
        mv(3000, cfg.left + pad, ly(r1), label_w, px(20.0));
        mv(ID_WORKSHOP, field_x, r1 + eoff, field_w - px(96.0), eh);
        mv(ID_BROWSE, field_x + field_w - px(88.0), r1, px(88.0), hi);

        let r2 = r1 + gap;
        mv(3001, cfg.left + pad, ly(r2), label_w, px(20.0));
        mv(ID_REMOTE, field_x, r2 + eoff, field_w, eh);

        let r3 = r2 + gap;
        mv(3002, cfg.left + pad, ly(r3), label_w, px(20.0));
        mv(ID_MIN, field_x, r3 + eoff, px(76.0), eh);
        mv(3100, field_x + px(80.0), ly(r3), px(44.0), px(20.0));
        mv(ID_MAX, field_x + px(128.0), r3 + eoff, px(76.0), eh);
        mv(3101, field_x + px(212.0), ly(r3), px(150.0), px(20.0));

        let r4 = r3 + gap;
        mv(3003, cfg.left + pad, ly(r4), label_w, px(20.0));
        // 与右侧 ADB 输入框视觉框上下对齐（视觉框顶部比控件顶部早 1px）
        mv(ID_CHANNEL, field_x, r4 - px(1.0), px(210.0), px(200.0));
        mv(3102, field_x + px(222.0), ly(r4), px(62.0), px(20.0));
        mv(ID_ADB, field_x + px(286.0), r4 + eoff, cfg.right - pad - field_x - px(286.0), eh);

        let r5 = r4 + gap;
        mv(ID_TRAY_CHK, field_x, r5, px(230.0), px(22.0));
        mv(ID_MP4_CHK, field_x + px(246.0), r5, px(230.0), px(22.0));

        // ---- 胶囊按钮行（卡片之间） ----
        let by = cfg.bottom + px(14.0);
        let bh = px(34.0);
        mv(ID_SAVE, cfg.left, by, px(112.0), bh);
        mv(ID_SYNC_NOW, cfg.left + px(120.0), by, px(96.0), bh);
        mv(ID_PAUSE_TRANSFER, cfg.left + px(224.0), by, px(84.0), bh);
        mv(ID_OPEN_WS, cfg.left + px(316.0), by, px(112.0), bh);
        mv(ID_OPEN_DATA, cfg.left + px(436.0), by, px(112.0), bh);
        let hint_x = cfg.left + px(558.0);
        mv(ID_SAVE_HINT, hint_x, by + px(8.0), (cfg.right - hint_x).max(px(60.0)), px(20.0));

        // ---- 卡片 2：运行状态 ----
        let sy = cards[1].top + px(16.0);
        let inner_w = cards[1].right - cards[1].left - pad * 2;
        mv(ID_STATUS, cards[1].left + pad + px(16.0), sy, inner_w - px(16.0), px(20.0));
        mv(ID_PROGRESS, cards[1].left + pad, sy + px(22.0), inner_w, px(20.0));
        mv(ID_STATS, cards[1].left + pad, sy + px(44.0), inner_w, px(20.0));

        // ---- 卡片 3：日志 ----
        mv(3004, log.left + pad, log.top + px(12.0), label_w, px(20.0));
        let log_y = log.top + px(36.0);
        mv(ID_LOG, log.left + pad, log_y, log.right - log.left - pad * 2,
           (log.bottom - px(14.0) - log_y).max(px(80.0)));

    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_buttons_are_mutually_exclusive() {
        // 未连接：两个都不能点
        assert_eq!(transfer_button_states(false, false, false), (false, false));
        assert_eq!(transfer_button_states(false, true, false), (false, false));
        // 空闲：只有「立即同步」
        assert_eq!(transfer_button_states(true, false, false), (true, false));
        // 有传输/排队：只有「暂停」
        assert_eq!(transfer_button_states(true, true, false), (false, true));
        // 已暂停：只有「继续」
        assert_eq!(transfer_button_states(true, false, true), (false, true));
        assert_eq!(transfer_button_states(true, true, true), (false, true));
    }

    #[test]
    fn channel_labels_match_config_values() {
        // 索引顺序必须与 collect_config 的映射一致，否则保存会串档
        assert_eq!(CHANNEL_LABELS.len(), 4);
        assert_eq!(CHANNEL_LABELS[1], "仅 MTP");
        assert_eq!(CHANNEL_LABELS[2], "仅 ADB");
        assert_eq!(CHANNEL_LABELS[3], "多通道（USB+WiFi）");
    }
}
