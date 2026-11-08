//! 传输通道抽象：MTP(WPD) 与 adb 两种实现，上层只依赖 trait。

pub mod adb;
pub mod mtp;
pub mod multipath;

use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// 用户点了「暂停」时 upload 返回的错误标记（引擎据此重新入队而不是计失败）。
pub const ERR_PAUSED: &str = "__PAUSED_BY_USER__";

pub fn paused() -> anyhow::Error {
    anyhow!(ERR_PAUSED)
}

pub fn is_paused(e: &anyhow::Error) -> bool {
    e.to_string().contains(ERR_PAUSED)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Mtp,
    Adb,
    /// USB(adb) + WiFi 双链路并行；底座仍是 adb，所以对外能力与 Adb 一致。
    Multipath,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Mtp => "mtp",
            Channel::Adb => "adb",
            Channel::Multipath => "multipath",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Channel::Mtp => "MTP",
            Channel::Adb => "ADB",
            Channel::Multipath => "ADB+WiFi",
        }
    }
}

/// 一个已就绪的传输会话（在同步线程内创建与使用，不跨线程转移）。
pub trait Transport {
    fn channel(&self) -> Channel;

    /// 设备可读名称，用于日志/界面。
    fn device_name(&self) -> String;

    /// 手机侧存储列表（MTP 会列出"内部存储/SD卡"，adb 只有一项）。
    fn storages(&self) -> Vec<String>;

    /// 检查目标目录可用，必要时创建（含多级）。
    fn ensure_dir(&mut self, remote_dir: &str) -> Result<()>;

    /// 轻量探活：通道仍可用返回 Ok，设备拔掉/切模式后返回 Err。
    ///
    /// 主循环按固定间隔调用它；不探活的话，已建立的会话在设备消失后不会自己失效，
    /// 界面会一直停在「已连接」。实现要尽量便宜（一次 IPC / 一次进程调用）。
    fn ping(&mut self) -> Result<()>;

    /// 列出目录下已存在的文件名与其大小（字节）。目录不存在返回空表。
    fn list_files(&mut self, remote_dir: &str) -> Result<Vec<(String, u64)>>;

    /// 上传一个文件。`abort` 置位表示用户要暂停：实现应尽快中止并清理半截文件。
    fn upload(
        &mut self,
        local: &Path,
        remote_dir: &str,
        remote_name: &str,
        abort: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()>;
}

/// 供界面显示用的通道状态。
#[derive(Debug, Clone)]
pub struct DeviceStatus {
    pub channel: Channel,
    pub name: String,
    pub storages: Vec<String>,
}

pub fn normalize_remote_dir(input: &str) -> Result<String> {
    let dir = crate::util::to_unix_path(input.trim());
    if dir.is_empty() {
        return Err(anyhow!("目标目录为空"));
    }
    let mut d = if dir.starts_with('/') {
        dir
    } else {
        format!("/{dir}")
    };
    while d.ends_with('/') && d.len() > 1 {
        d.pop();
    }
    Ok(d)
}
