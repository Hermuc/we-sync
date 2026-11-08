//! 配置：全部落在 exe 同级 `config.toml`（绿色可移植，不写注册表）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::util;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub general: General,
    pub source: Source,
    pub filter: Filter,
    pub target: Target,
    pub transport: Transport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// 启动后立即扫描并同步一次
    pub sync_on_start: bool,
    /// 关闭设置窗口时最小化到托盘（false = 直接退出）
    pub close_to_tray: bool,
    /// 设备轮询间隔（秒）
    pub device_poll_secs: u64,
    /// 新壁纸下载完成后额外等待（秒），应对 Steam 收尾写盘
    pub settle_delay_secs: u64,
    /// 单个文件传输失败重试次数
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    /// Wallpaper Engine 创意工坊内容目录；留空则自动探测 Steam 库
    pub workshop_dir: String,
    /// 只复制 mp4（video 壁纸）。关闭则按 project.json 的 file 字段判断
    pub mp4_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    /// 最小体积（MB），0 = 不限
    pub min_mb: f64,
    /// 最大体积（MB），0 = 不限
    pub max_mb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    /// 手机端目标目录（以 / 开头的 Android 路径）
    pub remote_dir: String,
    /// 文件名模板：{id} {title} {ext}
    pub name_template: String,
    /// 目标已存在同大小文件时跳过
    pub skip_existing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Transport {
    /// auto | mtp | adb | multipath
    ///
    /// `multipath` = USB(adb) + WiFi 两条链路并行；不可用时自动回落到 auto 的择优顺序。
    pub prefer: String,
    /// adb.exe 路径；留空则从 PATH 找
    pub adb_path: String,
    /// MTP 模式下选择哪个存储（0 = 第一个），对应界面上列出的存储序号
    pub mtp_storage_index: usize,
    /// 多通道（USB + WiFi 并行）的参数；是否启用由 `prefer = "multipath"` 决定
    pub multipath: Multipath,
}

/// 多通道传输配置。
///
/// 绿色约定：默认关闭；任何前置条件不满足都自动回落到单通道，不会让同步变慢。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Multipath {
    /// agent 与 PC 共用的端口（USB 侧由 adb forward 映射到本机回环）
    pub port: u16,
    /// 分块大小（KB）
    pub chunk_kb: u32,
    /// agent 二进制路径；留空则找 `plugins/agent/wsync-agent-<abi>`
    pub agent_path: String,
    /// 只有一条链路时也启用（仅调试用：不会更快，但能在真机上验证协议栈）
    pub allow_single_link: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            sync_on_start: true,
            close_to_tray: true,
            device_poll_secs: 5,
            settle_delay_secs: 8,
            max_retries: 2,
        }
    }
}

impl Default for Source {
    fn default() -> Self {
        Self {
            workshop_dir: String::new(),
            mp4_only: true,
        }
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            min_mb: 5.0,
            max_mb: 0.0,
        }
    }
}

impl Default for Target {
    fn default() -> Self {
        Self {
            remote_dir: "/sdcard/.WE".to_string(),
            name_template: "{title} [{id}].{ext}".to_string(),
            skip_existing: true,
        }
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            prefer: "auto".to_string(),
            adb_path: String::new(),
            mtp_storage_index: 0,
            multipath: Multipath::default(),
        }
    }
}

impl Default for Multipath {
    fn default() -> Self {
        Self {
            port: 5740,
            chunk_kb: 1024,
            agent_path: String::new(),
            allow_single_link: false,
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let path = util::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<AppConfig>(&text) {
                Ok(cfg) => {
                    crate::log_info!("配置已加载: {}", path.display());
                    cfg
                }
                Err(e) => {
                    crate::log_error!("配置解析失败({e})，使用默认值: {}", path.display());
                    let cfg = AppConfig::default();
                    cfg.save();
                    cfg
                }
            },
            Err(_) => {
                let cfg = AppConfig::default();
                cfg.save();
                crate::log_info!("已生成默认配置: {}", path.display());
                cfg
            }
        }
    }

    pub fn save(&self) {
        let path = util::config_path();
        if let Ok(text) = toml::to_string_pretty(self) {
            let body = format!(
                "# WE-Sync 配置文件（绿色便携：与 exe 同目录，删掉本文件即恢复默认）\n\
                 # 体积单位 MB，0 表示不限制；remote_dir 为手机端绝对路径\n\n{}",
                annotate(&text)
            );
            if let Err(e) = std::fs::write(&path, body) {
                crate::log_error!("写配置失败: {e}");
            }
        }
    }

    /// 有效的最小/最大字节数（0 表示无限制）。
    pub fn min_bytes(&self) -> u64 {
        (self.filter.min_mb.max(0.0) * util::MB_F64) as u64
    }

    pub fn max_bytes(&self) -> u64 {
        if self.filter.max_mb <= 0.0 {
            u64::MAX
        } else {
            (self.filter.max_mb * util::MB_F64) as u64
        }
    }

    pub fn adb_path_or_auto(&self) -> Option<PathBuf> {
        let p = self.transport.adb_path.trim();
        if !p.is_empty() {
            let path = PathBuf::from(p);
            if path.is_file() {
                return Some(path);
            }
        }
        crate::discover::which_adb()
    }
}

/// 给序列化出来的每一行补行内注释，保证用户直接读 config.toml 也能看懂。
fn annotate(toml_text: &str) -> String {
    let mut out = String::with_capacity(toml_text.len() + 512);
    for line in toml_text.lines() {
        let key = line.split('=').next().unwrap_or("").trim();
        if line.contains('=') {
            if let Some(c) = comment_for(key) {
                out.push_str(&format!("{line}  # {c}\n"));
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn comment_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "sync_on_start" => "启动后立即扫描同步一次",
        "close_to_tray" => "关闭窗口时隐藏到托盘（false = 直接退出）",
        "device_poll_secs" => "手机连接检测间隔（秒）",
        "settle_delay_secs" => "文件最近变动不足该秒数时先等待，避免传半成品",
        "max_retries" => "单个文件失败重试次数",
        "workshop_dir" => "留空 = 自动探测 Steam 库里的 431960 工坊目录",
        "mp4_only" => "只复制 mp4 视频壁纸",
        "min_mb" => "最小体积（MB），0 = 不限",
        "max_mb" => "最大体积（MB），0 = 不限",
        "remote_dir" => "手机端目标目录（Android 绝对路径）",
        "name_template" => "文件名模板，可用 {title} {id} {ext}",
        "skip_existing" => "手机上已有同名同大小文件则跳过",
        "prefer" => "auto（按实测速度择优）| mtp | adb | multipath（USB+WiFi 双链路并行）",
        "adb_path" => "adb.exe 路径，留空 = 自动从 PATH / tools\\adb.exe 查找",
        "mtp_storage_index" => "多存储设备时选第几个存储（0 起）",
        // [transport.multipath] 多通道
        "port" => "agent 端口，USB 侧由 adb forward 映射到本机回环",
        "chunk_kb" => "分块大小（KB），越大调度开销越低、尾块等待越久",
        "agent_path" => "agent 二进制路径，留空 = 找 plugins\\agent\\wsync-agent-<abi>",
        "allow_single_link" => "仅一条链路也启用（调试用，默认 false）",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotate_keeps_comments() {
        let text = toml::to_string_pretty(&AppConfig::default()).unwrap();
        let out = annotate(&text);
        assert!(out.contains("min_mb = 5.0  # 最小体积"));
        assert!(out.contains("remote_dir"));
        // 注释不应破坏 TOML 结构
        let back: AppConfig = toml::from_str(&out).expect("带注释的配置仍应可解析");
        assert_eq!(back.filter.min_mb, 5.0);
        assert_eq!(back.target.remote_dir, "/sdcard/.WE");
    }

    #[test]
    fn byte_bounds() {
        let mut c = AppConfig::default();
        c.filter.min_mb = 50.0;
        c.filter.max_mb = 0.0;
        assert_eq!(c.min_bytes(), 50 * 1024 * 1024);
        assert_eq!(c.max_bytes(), u64::MAX);
        c.filter.max_mb = 200.0;
        assert_eq!(c.max_bytes(), 200 * 1024 * 1024);
    }

    #[test]
    fn remote_dir_normalize() {
        use crate::transport::normalize_remote_dir;
        assert_eq!(normalize_remote_dir("/sdcard/.WE/").unwrap(), "/sdcard/.WE");
        assert_eq!(normalize_remote_dir("sdcard/.WE").unwrap(), "/sdcard/.WE");
        assert!(normalize_remote_dir("  ").is_err());
    }
}
