//! 通用工具：绿色路径解析、日志、单实例、时间与文件稳定性判定。

pub mod proc;

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 应用目录：默认 exe 所在目录；集成方（如 WEaveR）可在首次使用前覆盖。
static APP_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 覆盖应用目录（config/state/log 都落在这里）。返回 false 表示已被初始化过。
pub fn set_app_dir(dir: PathBuf) -> bool {
    APP_DIR.set(dir).is_ok()
}

/// 便携根目录 = exe 所在目录（未调用 set_app_dir 时）。绝不写注册表 / %APPDATA%。
pub fn app_dir() -> PathBuf {
    APP_DIR
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .clone()
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

pub fn state_path() -> PathBuf {
    app_dir().join("sync-state.json")
}

pub fn log_path() -> PathBuf {
    app_dir().join("logs").join("we-sync.log")
}

// ---------------------------------------------------------------- 日志

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// 日志级别：环境变量 `WE_SYNC_LOG=debug|info|warn|error`，默认 info。
    ///
    /// 之前级别写死在 Info，`log_debug!` 永远打不出来 —— 排查多通道这类问题时
    /// 只能重新编译。现在临时设一次环境变量就能看到详细日志。
    fn from_env() -> Level {
        match std::env::var("WE_SYNC_LOG")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "debug" | "trace" => Level::Debug,
            "warn" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

struct Logger {
    file: Mutex<Option<File>>,
    ring: Mutex<VecDeque<String>>,
    level: Mutex<Level>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| Logger {
        file: Mutex::new(open_log_file()),
        ring: Mutex::new(VecDeque::with_capacity(256)),
        level: Mutex::new(Level::from_env()),
    })
}

fn open_log_file() -> Option<File> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // 简易滚动：> 4 MiB 就改名 .1
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > 4 * 1024 * 1024 {
            let _ = fs::rename(&path, path.with_extension("log.1"));
        }
    }
    OpenOptions::new().create(true).append(true).open(&path).ok()
}

pub fn log(level: Level, msg: impl AsRef<str>) {
    let lg = logger();
    if level < *lg.level.lock().unwrap() {
        return;
    }
    let line = format!("[{}] [{}] {}", now_string(), level.tag(), msg.as_ref());
    {
        let mut ring = lg.ring.lock().unwrap();
        ring.push_back(line.clone());
        while ring.len() > 300 {
            ring.pop_front();
        }
    }
    if let Some(file) = lg.file.lock().unwrap().as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

/// 最近日志（供设置窗口展示）。
pub fn recent_logs(n: usize) -> Vec<String> {
    let ring = logger().ring.lock().unwrap();
    let skip = ring.len().saturating_sub(n);
    ring.iter().skip(skip).cloned().collect()
}

#[macro_export]
macro_rules! log_info {
    ($($a:tt)*) => { $crate::util::log($crate::util::Level::Info, format!($($a)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($($a:tt)*) => { $crate::util::log($crate::util::Level::Warn, format!($($a)*)) };
}
#[macro_export]
macro_rules! log_error {
    ($($a:tt)*) => { $crate::util::log($crate::util::Level::Error, format!($($a)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($($a:tt)*) => { $crate::util::log($crate::util::Level::Debug, format!($($a)*)) };
}

// ---------------------------------------------------------------- 时间

/// 当前 epoch 秒：日志时间戳与文件稳定判定共用同一时间源。
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 1 MiB 的字节数：体积阈值与吞吐统计共用同一换算口径。
pub const MB_F64: f64 = 1_048_576.0;

pub fn now_string() -> String {
    format_epoch_local(epoch_secs())
}

/// 极简本地时间格式化（只用于日志/状态，精度到秒即可）。
fn format_epoch_local(epoch: u64) -> String {
    let (y, mo, d, h, mi, s) = epoch_to_local(epoch);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 本地时区偏移（分钟，含夏令时）。
fn tz_offset_minutes() -> i64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        unsafe {
            let mut tzi = TIME_ZONE_INFORMATION::default();
            let id = GetTimeZoneInformation(&mut tzi);
            let mut bias = tzi.Bias as i64;
            // 2 = TIME_ZONE_ID_DAYLIGHT
            if id == 2 {
                bias += tzi.DaylightBias as i64;
            } else {
                bias += tzi.StandardBias as i64;
            }
            return -bias; // local = utc - bias
        }
    }
    #[allow(unreachable_code)]
    0
}

/// epoch 秒 -> 本地时间元组（不依赖 CRT）。
fn epoch_to_local(epoch: u64) -> (i32, u32, u32, u32, u32, u32) {
    let local = epoch as i64 + tz_offset_minutes() * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// Howard Hinnant civil_from_days 算法。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

pub fn unix_now() -> u64 {
    epoch_secs()
}

// ---------------------------------------------------------------- 文件工具

/// 判断文件是否"下载完成"：两次采样大小一致，且能以独占方式打开。
pub fn is_file_settled(path: &Path, prev: Option<u64>) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let size = meta.len();
    if size == 0 {
        return false;
    }
    if let Some(p) = prev {
        if p != size {
            return false;
        }
    }
    // 能独占打开 => 没有其它进程还在写
    OpenOptions::new().read(true).write(true).open(path).is_ok()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 清理文件名中的非法字符（跨 MTP / FAT32 都安全）。
pub fn sanitize_file_name(name: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let bad = matches!(
            ch,
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\r' | '\n' | '\t'
        ) || (ch as u32) < 0x20;
        out.push(if bad { '_' } else { ch });
    }
    let trimmed = out.trim().trim_end_matches('.').to_string();
    let mut s: String = trimmed.chars().take(max_len).collect();
    if s.is_empty() {
        s.push_str("wallpaper");
    }
    s
}

/// 统一把外部输入的反斜杠路径转成正斜杠（MTP/adb 用）。
pub fn to_unix_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// 文件修改时间（epoch 秒）：同步状态与「等待下载完成」判定共用。
pub fn mtime_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
