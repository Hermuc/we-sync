//! 同步状态：记录已推送的壁纸，避免重复传输。与 exe 同目录的 sync-state.json。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::util;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub items: BTreeMap<String, SyncedItem>,
    /// 各通道的累计实测吞吐（auto 模式据此选更快的通道）
    #[serde(default)]
    pub channel_stats: BTreeMap<String, ChannelStat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelStat {
    pub bytes: f64,
    pub secs: f64,
}

impl ChannelStat {
    pub fn mbps(&self) -> f64 {
        if self.secs <= 0.0 {
            0.0
        } else {
            self.bytes / util::MB_F64 / self.secs
        }
    }
}

fn default_version() -> u32 {
    1
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            version: default_version(),
            items: BTreeMap::new(),
            channel_stats: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedItem {
    /// 手机端文件名
    pub remote_name: String,
    /// 源文件大小（字节）
    pub size: u64,
    /// 源文件修改时间（epoch 秒）
    #[serde(default)]
    pub mtime: u64,
    /// 同步完成时间（可读字符串）
    #[serde(default)]
    pub synced_at: String,
    /// 使用的通道：mtp / adb
    #[serde(default)]
    pub via: String,
}

impl SyncState {
    pub fn load() -> Self {
        let path = util::state_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                crate::log_warn!("状态文件损坏({e})，重建: {}", path.display());
                SyncState::default()
            }),
            Err(_) => SyncState::default(),
        }
    }

    pub fn save(&self) {
        let path = util::state_path();
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            Err(e) => crate::log_error!("序列化状态失败: {e}"),
        }
    }

    /// 是否需要（重新）推送：未记录、大小变化、或目标文件名变化。
    pub fn needs_sync(&self, id: &str, remote_name: &str, size: u64) -> bool {
        match self.items.get(id) {
            None => true,
            Some(it) => it.size != size || it.remote_name != remote_name,
        }
    }

    pub fn mark(&mut self, id: &str, remote_name: &str, size: u64, mtime: u64, via: &str) {
        self.items.insert(
            id.to_string(),
            SyncedItem {
                remote_name: remote_name.to_string(),
                size,
                mtime,
                synced_at: util::now_string(),
                via: via.to_string(),
            },
        );
    }

    /// 源壁纸已被取消订阅/删除时清理记录。
    pub fn retain_ids(&mut self, alive: &[String]) {
        self.items.retain(|k, _| alive.iter().any(|a| a == k));
    }

    /// 记录一次成功传输的实测速度。
    pub fn record_speed(&mut self, channel: &str, bytes: u64, secs: f64) {
        if secs <= 0.0 || bytes == 0 {
            return;
        }
        let e = self.channel_stats.entry(channel.to_string()).or_default();
        e.bytes += bytes as f64;
        e.secs += secs;
    }

    /// 通道历史平均速度（MB/s）；没有样本返回 None。
    pub fn channel_mbps(&self, channel: &str) -> Option<f64> {
        self.channel_stats
            .get(channel)
            .filter(|s| s.secs > 0.5 && s.bytes > 0.0)
            .map(|s| s.mbps())
    }

}
