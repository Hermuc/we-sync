//! 环境探测：定位 adb.exe、Steam 库与 Wallpaper Engine 工坊目录。
//!
//! 与 `config` 的配置读写 / 默认值分离，避免配置模块承担系统发现职责。

use crate::config::AppConfig;
use crate::util;
use std::path::{Path, PathBuf};

/// 从 PATH 里找 adb.exe（绿色环境下也支持 exe 同级 tools\adb.exe）。
pub fn which_adb() -> Option<PathBuf> {
    let local = util::app_dir().join("tools").join("adb.exe");
    if local.is_file() {
        return Some(local);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("adb.exe");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 自动探测 Steam 库里的 Wallpaper Engine 工坊目录。
pub fn detect_workshop_dir() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for key in [
        r"SOFTWARE\WOW6432Node\Valve\Steam",
        r"SOFTWARE\Valve\Steam",
    ] {
        if let Some(p) = reg_string(windows::Win32::System::Registry::HKEY_LOCAL_MACHINE, key, "InstallPath") {
            roots.push(PathBuf::from(p));
        }
    }
    if let Some(p) = reg_string(
        windows::Win32::System::Registry::HKEY_CURRENT_USER,
        r"Software\Valve\Steam",
        "SteamPath",
    ) {
        roots.push(PathBuf::from(p.replace('/', "\\")));
    }

    let mut libraries: Vec<PathBuf> = roots.clone();
    for root in &roots {
        let vdf = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(text) = std::fs::read_to_string(&vdf) {
            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("\"path\"") {
                    continue;
                }
                if let Some(v) = line.split('"').filter(|s| !s.trim().is_empty()).nth(1) {
                    libraries.push(PathBuf::from(v.replace("\\\\", "\\")));
                }
            }
        }
    }

    let mut found: Option<PathBuf> = None;
    for lib in libraries {
        let dir = lib.join("steamapps").join("workshop").join("content").join("431960");
        if dir.is_dir() {
            // 优先选壁纸数量最多的库
            let n = std::fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0);
            if n > 0 || found.is_none() {
                found = Some(dir);
            }
        }
    }
    found
}

fn reg_string(
    root: windows::Win32::System::Registry::HKEY,
    sub: &str,
    name: &str,
) -> Option<String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};
    let sub = HSTRING::from(sub);
    let name = HSTRING::from(name);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        let rc = RegGetValueW(
            root,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        );
        if rc.is_ok() {
            let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 解析后的有效源目录。
pub fn effective_workshop_dir(cfg: &AppConfig) -> Option<PathBuf> {
    let manual = cfg.source.workshop_dir.trim();
    if !manual.is_empty() {
        let p = Path::new(manual);
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
    }
    detect_workshop_dir()
}

/// Steam 正在下载 431960 的标记目录。
pub fn steam_downloading_dir(workshop_dir: &Path) -> Option<PathBuf> {
    // <lib>/steamapps/workshop/content/431960 -> <lib>/steamapps/downloading/431960
    let steamapps = workshop_dir.parent()?.parent()?.parent()?;
    let d = steamapps.join("downloading").join("431960");
    if d.is_dir() {
        Some(d)
    } else {
        None
    }
}
