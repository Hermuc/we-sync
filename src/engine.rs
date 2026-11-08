//! 同步引擎：跑在独立线程里，负责设备探测、扫描差异、传输、状态持久化。
//!
//! 上层（UI/托盘）只通过 `Cmd` 下令、通过 `Event` 收状态。

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::AppConfig;
use crate::discover;
use crate::model::{self, Wallpaper};
use crate::state::SyncState;
use crate::util::mtime_secs;
use crate::transport::{
    adb::AdbTransport, mtp::MtpTransport, multipath::MultipathTransport, Channel, DeviceStatus,
    Transport,
};

/// 已连接通道的探活间隔。手机拔线后最多这么久，界面就会切回「未连接」。
///
/// 1.5s：ping 只是一次轻量 IPC（adb get-state / MTP 枚举根目录），
/// 成本远低于用户的等待感；再长会让指示灯「灭得不及时」。
const PING_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub enum Event {
    Log(String),
    Device(Option<DeviceStatus>),
    Busy(bool),
    Progress {
        name: String,
        done: u64,
        total: u64,
    },
    Stats(Stats),
    Queue(usize),
    /// 传输是否处于暂停状态
    TransferPaused(bool),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub uploaded: u64,
    pub skipped: u64,
    pub failed: u64,
    pub filtered: u64,
}

#[derive(Debug, Clone)]
pub enum Cmd {
    SyncNow,
    /// 暂停整个同步（扫描 + 传输）
    SetPaused(bool),
    /// 暂停/恢复传输（当前文件会被中断并重新排队）
    SetTransferPaused(bool),
    SetConfig(Box<AppConfig>),
    Quit,
}

pub struct EngineHandle {
    pub tx: Sender<Cmd>,
    pub dirty: Arc<AtomicBool>,
    /// 实时统计（worker 每次变化即写入，UI 直接读，不依赖事件队列）
    pub stats: Arc<Mutex<Stats>>,
    /// 实时待处理数量
    pub queue: Arc<AtomicUsize>,
    /// 立即中止当前传输（UI 直接置位，不必等 worker 处理命令）
    pub abort: Arc<AtomicBool>,
}

/// 启动工作线程。
pub fn spawn(cfg: AppConfig, evt: Sender<Event>) -> EngineHandle {
    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
    let dirty = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let queue = Arc::new(AtomicUsize::new(0));
    let abort = Arc::new(AtomicBool::new(false));

    let dirty2 = dirty.clone();
    let stats2 = stats.clone();
    let queue2 = queue.clone();
    let abort2 = abort.clone();
    std::thread::Builder::new()
        .name("we-sync-worker".into())
        .spawn(move || worker(cfg, evt, rx, dirty2, stats2, queue2, abort2))
        .expect("无法创建同步线程");

    EngineHandle {
        tx,
        dirty,
        stats,
        queue,
        abort,
    }
}

/// 选路：auto 时按历史实测吞吐择优（差值需超过 5% 才换通道，避免抖动）。
pub fn order_channels(prefer: &str, mtp_mbps: Option<f64>, adb_mbps: Option<f64>) -> Vec<Channel> {
    match prefer.to_lowercase().as_str() {
        "adb" => vec![Channel::Adb],
        "mtp" => vec![Channel::Mtp],
        _ => {
            let adb_faster = matches!((mtp_mbps, adb_mbps), (Some(m), Some(a)) if a > m * 1.05);
            if adb_faster {
                vec![Channel::Adb, Channel::Mtp]
            } else {
                vec![Channel::Mtp, Channel::Adb]
            }
        }
    }
}

struct Worker {
    cfg: AppConfig,
    state: SyncState,
    evt: Sender<Event>,
    transport: Option<Box<dyn Transport>>,
    device: Option<DeviceStatus>,
    pending: VecDeque<Wallpaper>,
    /// 已下发的壁纸 id，避免同一轮重复排队
    queued: HashSet<String>,
    paused: bool,
    last_scan: Option<Instant>,
    last_device_check: Option<Instant>,
    /// 上次探活时间：已连接的通道要定期 ping，否则拔线后界面一直停在「已连接」
    last_ping: Option<Instant>,
    /// 多通道探测失败后的冷却时间（避免每 2 秒重复投放 agent）
    mp_retry_at: Option<Instant>,
    last_usb_absent_log: Option<Instant>,
    stats: Stats,
    stats_shared: Arc<Mutex<Stats>>,
    queue_shared: Arc<AtomicUsize>,
    abort: Arc<AtomicBool>,
    transfer_paused: bool,
}

fn worker(
    cfg: AppConfig,
    evt: Sender<Event>,
    rx: Receiver<Cmd>,
    dirty: Arc<AtomicBool>,
    stats_shared: Arc<Mutex<Stats>>,
    queue_shared: Arc<AtomicUsize>,
    abort: Arc<AtomicBool>,
) {
    // WPD 需要 COM；MTA 适合后台线程
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut w = Worker {
        cfg,
        state: SyncState::load(),
        evt: evt.clone(),
        transport: None,
        device: None,
        pending: VecDeque::new(),
        queued: HashSet::new(),
        paused: false,
        last_scan: None,
        last_device_check: None,
        last_ping: None,
        mp_retry_at: None,
        last_usb_absent_log: None,
        stats: Stats::default(),
        stats_shared,
        queue_shared,
        abort,
        transfer_paused: false,
    };

    if w.cfg.general.sync_on_start {
        w.last_scan = None; // 立即扫描
    }

    loop {
        // 命令
        loop {
            match rx.try_recv() {
                Ok(Cmd::Quit) => {
                    crate::log_info!("同步线程退出");
                    w.state.save();
                    unsafe {
                        use windows::Win32::System::Com::CoUninitialize;
                        CoUninitialize();
                    }
                    return;
                }
                Ok(Cmd::SyncNow) => {
                    crate::log_info!("收到手动同步请求");
                    w.last_device_check = None;
                    w.last_scan = None;
                    w.pending.clear();
                    w.queued.clear();
                }
                Ok(Cmd::SetPaused(p)) => {
                    w.paused = p;
                    crate::log_info!("同步{}", if p { "已暂停" } else { "已恢复" });
                }
                Ok(Cmd::SetTransferPaused(p)) => {
                    w.transfer_paused = p;
                    w.abort.store(p, Ordering::Relaxed);
                    crate::log_info!("传输{}", if p { "已暂停" } else { "已继续" });
                    let _ = w.evt.send(Event::TransferPaused(p));
                }
                Ok(Cmd::SetConfig(cfg)) => {
                    crate::log_info!("配置已更新");
                    w.cfg = *cfg;
                    w.last_scan = None;
                    w.pending.clear();
                    w.queued.clear();
                    w.transport = None;
                    w.device = None;
                    w.last_device_check = None;
                    w.last_ping = None;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // 1) 设备：连接 / 探活。暂停同步时也要跑，否则暂停期间拔线界面不会变
        w.keep_alive();

        if w.paused {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }

        if dirty.swap(false, Ordering::SeqCst) {
            // 工坊目录有变化，尽快重扫
            w.last_scan = None;
        }

        // 2) 扫描工坊
        let poll = Duration::from_secs(w.cfg.general.device_poll_secs.max(1));
        let need_scan = w.last_scan.map(|t| t.elapsed() >= poll).unwrap_or(true);
        if w.transport.is_some() && need_scan {
            w.scan();
            w.last_scan = Some(Instant::now());
        }

        // 3) 传输
        if w.transport.is_some() {
            w.pump();
        }

        w.publish();
        let _ = w.evt.send(Event::Stats(w.stats));
        let _ = w.evt.send(Event::Queue(w.pending.len()));

        std::thread::sleep(Duration::from_millis(250));
    }
}

impl Worker {
    /// 把当前统计/队列长度立即写进共享状态（UI 每 250ms 读一次）。
    fn publish(&self) {
        if let Ok(mut s) = self.stats_shared.lock() {
            *s = self.stats;
        }
        self.queue_shared.store(self.pending.len(), Ordering::Relaxed);
    }

    fn emit_log(&self, msg: impl Into<String>) {
        let m = msg.into();
        crate::log_info!("{m}");
        let _ = self.evt.send(Event::Log(m));
    }

    fn disconnect(&mut self, reason: &str) {
        if self.device.is_some() {
            self.emit_log(format!("设备已断开: {reason}"));
        }
        self.transport = None;
        self.device = None;
        self.last_ping = None;
        // 重新插线后立刻重试多通道，不必等冷却到期
        self.mp_retry_at = None;
        let _ = self.evt.send(Event::Device(None));
        self.last_device_check = Some(Instant::now());
    }

    /// 设备保活：没连上就尝试连接，已连上就按时探活。
    ///
    /// 连接会话不会因为手机拔掉而自己失效，不主动 ping 界面就会一直停在「已连接」。
    fn keep_alive(&mut self) {
        if self.transport.is_none() {
            self.try_connect();
            return;
        }
        let due = self
            .last_ping
            .map(|t| t.elapsed() >= PING_INTERVAL)
            .unwrap_or(true);
        if due {
            self.last_ping = Some(Instant::now());
            self.check_alive();
        }
    }

    /// 已连接通道的存活检查：ping 一次，失败再确认一次才判定掉线。
    ///
    /// 只 ping 一次就断开容易误伤 —— USB 短暂拥塞、手机锁屏解锁切换 MTP 会话时，
    /// 单次 IPC 超时很常见；连续两次失败（间隔 150ms）才认为设备真的没了。
    fn check_alive(&mut self) {
        fn ping_once(w: &mut Worker) -> anyhow::Result<()> {
            match w.transport.as_mut() {
                Some(t) => t.ping(),
                None => Ok(()),
            }
        }
        if ping_once(self).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
        if let Err(e) = ping_once(self) {
            self.disconnect(&format!("设备无响应: {e}"));
        }
    }

    /// 尝试建立通道：auto = MTP 优先，其次 adb。
    fn try_connect(&mut self) {
        let now = Instant::now();
        if let Some(t) = self.last_device_check {
            if t.elapsed() < Duration::from_secs(2) {
                return;
            }
        }
        self.last_device_check = Some(now);

        let prefer = self.cfg.transport.prefer.clone();
        let mtp_mbps = self.state.channel_mbps("mtp");
        let adb_mbps = self.state.channel_mbps("adb");
        if prefer.eq_ignore_ascii_case("auto") {
            crate::log_debug!(
                "auto 选路: MTP={mtp_mbps:?} ADB={adb_mbps:?} MB/s"
            );
        }
        let candidates = order_channels(&prefer, mtp_mbps, adb_mbps);

        // ① 多通道优先：只有用户在下拉里显式选了「多通道」才会走这里。
        //    两条链路都通才命中；失败后进入冷却，避免每 2 秒重复往手机投放 agent。
        if prefer.eq_ignore_ascii_case("multipath") {
            let cooling = self
                .mp_retry_at
                .map(|t| Instant::now() < t)
                .unwrap_or(false);
            if !cooling {
                match self.try_multipath() {
                    Some(tr) => {
                        if self.adopt(tr) {
                            return;
                        }
                    }
                    None => {
                        self.mp_retry_at = Some(Instant::now() + Duration::from_secs(300));
                    }
                }
            }
        }

        for ch in candidates {
            let tr: Option<Box<dyn Transport>> = match ch {
                Channel::Mtp => MtpTransport::detect(self.cfg.transport.mtp_storage_index)
                    .map(|t| Box::new(t) as Box<dyn Transport>),
                Channel::Adb => self
                    .cfg
                    .adb_path_or_auto()
                    .and_then(AdbTransport::detect)
                    .map(|t| Box::new(t) as Box<dyn Transport>),
                // multipath 在上面的分支单独构造，不参与候选排序
                Channel::Multipath => None,
            };
            let Some(tr) = tr else { continue };
            if self.adopt(tr) {
                return;
            }
        }

        // 没设备：低频日志，避免刷屏
        let should_log = self
            .last_usb_absent_log
            .map(|t| t.elapsed() >= Duration::from_secs(60))
            .unwrap_or(true);
        if should_log {
            self.last_usb_absent_log = Some(Instant::now());
            crate::log_debug!("未检测到手机（MTP/adb 均无）");
        }
    }

    /// 通道建立成功后的统一收尾：建目录 → 上报状态 → 记为本轮连接。
    fn adopt(&mut self, mut tr: Box<dyn Transport>) -> bool {
        let remote = match crate::transport::normalize_remote_dir(&self.cfg.target.remote_dir) {
            Ok(d) => d,
            Err(e) => {
                self.emit_log(format!("目标目录配置无效: {e}"));
                return false;
            }
        };
        if let Err(e) = tr.ensure_dir(&remote) {
            self.emit_log(format!("{} 通道创建目录失败: {e}", tr.channel().label()));
            return false;
        }
        let status = DeviceStatus {
            channel: tr.channel(),
            name: tr.device_name(),
            storages: tr.storages(),
        };
        self.emit_log(format!(
            "已连接 [{}] {}，目标目录 {}",
            status.channel.label(),
            status.name,
            remote
        ));
        let _ = self.evt.send(Event::Device(Some(status.clone())));
        self.device = Some(status);
        self.transport = Some(tr);
        self.last_scan = None;
        self.last_ping = Some(Instant::now());
        true
    }

    /// 尝试启用多通道。前置条件任一不满足都返回 None，调用方静默回落单通道。
    fn try_multipath(&mut self) -> Option<Box<dyn Transport>> {
        let adb = self.cfg.adb_path_or_auto().and_then(AdbTransport::detect)?;
        let remote = crate::transport::normalize_remote_dir(&self.cfg.target.remote_dir).ok()?;
        match MultipathTransport::connect(adb, &self.cfg.transport.multipath, &remote) {
            Ok(t) => {
                crate::log_info!("多通道已启用: {}", t.link_labels().join(" + "));
                Some(Box::new(t))
            }
            Err(e) => {
                crate::log_info!("多通道不可用，回落单通道: {e}");
                None
            }
        }
    }

    /// 扫描工坊目录，把满足条件的壁纸排进队列。
    fn scan(&mut self) {
        let Some(dir) = discover::effective_workshop_dir(&self.cfg) else {
            self.emit_log("未找到 Wallpaper Engine 工坊目录，请在设置里指定");
            return;
        };
        // Steam 正在下载 => 等下一轮
        if discover::steam_downloading_dir(&dir).is_some() {
            crate::log_debug!("Steam 正在下载 Wallpaper Engine 内容，稍后重试");
            return;
        }

        let all = model::scan(&dir);
        let (min, max) = (self.cfg.min_bytes(), self.cfg.max_bytes());
        let mut alive: Vec<String> = Vec::with_capacity(all.len());
        let mut filtered_now = 0u64;
        // 本轮新发现的壁纸先攒起来，按体积从小到大入队（小文件先传完）
        let mut batch: Vec<Wallpaper> = Vec::new();

        for w in all {
            alive.push(w.id.clone());
            if self.cfg.source.mp4_only && !model::is_mp4(&w) {
                continue;
            }
            if !model::passes_size_filter(w.size, min, max) {
                filtered_now += 1;
                crate::log_debug!(
                    "跳过(体积 {} 不在区间): {} {}",
                    crate::util::human_size(w.size),
                    w.title,
                    w.id
                );
                continue;
            }
            // 刚变动过的文件先等一会儿，避免把下载中的半成品传上去
            let settle = self.cfg.general.settle_delay_secs;
            if settle > 0 {
                let age = crate::util::unix_now().saturating_sub(mtime_secs(&w.source));
                if age < settle {
                    crate::log_debug!("文件最近有变动，等待稳定({age}s < {settle}s)");
                    continue;
                }
            }
            // 文件仍被占用（Steam 还在写）=> 下一轮
            if !crate::util::is_file_settled(&w.source, None) {
                crate::log_debug!("文件未落盘完成，稍后再传: {}", w.source.display());
                continue;
            }
            let name = w.remote_name(&self.cfg.target.name_template);
            if !self.state.needs_sync(&w.id, &name, w.size) {
                continue;
            }
            if self.queued.contains(&w.id) {
                continue;
            }
            self.queued.insert(w.id.clone());
            batch.push(w);
        }

        self.stats.filtered = filtered_now;
        self.state.retain_ids(&alive);

        if !batch.is_empty() {
            batch.sort_by_key(|w| w.size);
            let preview: Vec<String> = batch
                .iter()
                .take(3)
                .map(|w| crate::util::human_size(w.size))
                .collect();
            let added = batch.len();
            for w in batch {
                self.pending.push_back(w);
            }
            self.publish();
            self.emit_log(format!(
                "发现 {added} 个待同步壁纸，按体积从小到大排队（{} …）",
                preview.join(" → ")
            ));
        }
    }

    /// 逐个上传队列里的文件。
    fn pump(&mut self) {
        // 暂停中：不动队列（点「继续」后才恢复）
        if self.transfer_paused || self.abort.load(Ordering::Relaxed) {
            return;
        }
        while !self.transfer_paused && !self.abort.load(Ordering::Relaxed) {
            let Some(w) = self.pending.pop_front() else { break };
            self.queued.remove(&w.id);
            self.publish();
            let remote_dir = match crate::transport::normalize_remote_dir(&self.cfg.target.remote_dir)
            {
                Ok(d) => d,
                Err(_) => return,
            };
            let name = w.remote_name(&self.cfg.target.name_template);

            // 手机上已有同名同大小文件 => 直接记为已同步
            if self.cfg.target.skip_existing {
                let existing = self
                    .transport
                    .as_mut()
                    .and_then(|tr| tr.list_files(&remote_dir).ok())
                    .map(|list| list.iter().any(|(n, sz)| n == &name && *sz == w.size))
                    .unwrap_or(false);
                if existing {
                    let mtime = mtime_secs(&w.source);
                    self.state.mark(&w.id, &name, w.size, mtime, "exist");
                    self.state.save();
                    self.stats.skipped += 1;
                    self.publish();
                    self.emit_log(format!("手机上已存在，跳过 {name}"));
                    continue;
                }
            }

            let started = Instant::now();
            let _ = self.evt.send(Event::Busy(true));
            let display = format!("{} [{}] {}", w.title, w.id, crate::util::human_size(w.size));
            self.emit_log(format!("开始传输 {display}"));

            let mut last_sent = Instant::now();
            let evt_prog = self.evt.clone();
            let mut prog = |done: u64, total: u64| {
                if last_sent.elapsed() >= Duration::from_millis(300) || done >= total {
                    last_sent = Instant::now();
                    let _ = evt_prog.send(Event::Progress {
                        name: name.clone(),
                        done,
                        total,
                    });
                }
            };

            let mut attempt = 0;
            let mut ok = false;
            let mut fatal = false;
            let mut interrupted = false;
            let abort = self.abort.clone();
            while attempt <= self.cfg.general.max_retries {
                let Some(tr) = self.transport.as_mut() else {
                    fatal = true;
                    break;
                };
                match tr.upload(&w.source, &remote_dir, &name, &abort, &mut prog) {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        if crate::transport::is_paused(&e) {
                            interrupted = true;
                            break;
                        }
                        attempt += 1;
                        self.emit_log(format!("传输失败(第 {attempt} 次): {e}"));
                        if attempt > self.cfg.general.max_retries {
                            break;
                        }
                        // 通道可能已失效，重建后再试
                        self.disconnect("传输异常，重新探测");
                        std::thread::sleep(Duration::from_millis(800));
                        self.try_connect();
                        if self.transport.is_none() {
                            fatal = true;
                            break;
                        }
                    }
                }
            }

            if interrupted {
                // 用户暂停：把这个文件放回队头，等「继续」时重传整份
                self.transfer_paused = true;
                self.abort.store(true, Ordering::Relaxed);
                self.queued.insert(w.id.clone());
                self.pending.push_front(w);
                self.publish();
                let _ = self.evt.send(Event::Busy(false));
                let _ = self.evt.send(Event::TransferPaused(true));
                self.emit_log(format!("传输已暂停（{name} 将从头重传）"));
                return;
            }

            if ok {
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let mbps = (w.size as f64 / crate::util::MB_F64) / secs;
                let mtime = mtime_secs(&w.source);
                let via = self
                    .device
                    .as_ref()
                    .map(|d| d.channel.as_str())
                    .unwrap_or("mtp");
                self.state.record_speed(via, w.size, secs);
                crate::log_info!(
                    "耗时 {secs:.1}s，平均 {mbps:.1} MB/s（{}）",
                    via.to_uppercase()
                );
                self.state.mark(&w.id, &name, w.size, mtime, via);
                self.state.save();
                self.stats.uploaded += 1;
                self.publish();
                self.emit_log(format!("已复制 {name}"));
            } else {
                self.stats.failed += 1;
                self.publish();
                self.emit_log(format!("放弃 {name}"));
                if fatal {
                    let _ = self.evt.send(Event::Busy(false));
                    return;
                }
            }
            let _ = self.evt.send(Event::Busy(false));
        }
    }
}

/// 目录监听：变化时把 dirty 置位（worker 下一轮重扫）。
pub fn spawn_watcher(dir: PathBuf, dirty: Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if matches!(
                ev.kind,
                notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
            ) {
                dirty.store(true, Ordering::SeqCst);
            }
        }
    })
    .ok()?;
    watcher.watch(&dir, RecursiveMode::Recursive).ok()?;
    crate::log_info!("已监听工坊目录: {}", dir.display());
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_faster_channel() {
        // 无样本：MTP 优先（免开调试）
        assert_eq!(order_channels("auto", None, None), vec![Channel::Mtp, Channel::Adb]);
        // 本机实测：MTP 37、ADB 31 -> 仍选 MTP
        assert_eq!(
            order_channels("auto", Some(37.3), Some(31.3)),
            vec![Channel::Mtp, Channel::Adb]
        );
        // ADB 明显更快 -> 换 ADB
        assert_eq!(
            order_channels("auto", Some(9.0), Some(35.0)),
            vec![Channel::Adb, Channel::Mtp]
        );
        // 差距不足 5% 不换，避免来回抖
        assert_eq!(
            order_channels("auto", Some(30.0), Some(31.0)),
            vec![Channel::Mtp, Channel::Adb]
        );
        // 强制单通道
        assert_eq!(order_channels("adb", None, None), vec![Channel::Adb]);
        assert_eq!(order_channels("mtp", None, None), vec![Channel::Mtp]);
    }
}
