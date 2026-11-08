//! 多通道传输：ADB(USB) + WiFi 两条链路并行送同一个文件。
//!
//! 层次：
//! - `planner`   决定用哪几条链路（纯逻辑）
//! - `scheduler` 发块号 / 重派 / 进度（纯逻辑）
//! - `link`      单条 TCP 链路与按 offset 读文件
//! - `agent`     agent 的投放、启动、清理（唯一依赖 adb 的地方）
//!
//! 上层只看到 `Transport` 的实现，`engine` 一行都不用改。

mod agent;
#[cfg(test)]
mod e2e;
mod link;
mod planner;
mod scheduler;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use wsync_proto::{new_token, Sha256, TOKEN_LEN};

use super::adb::AdbTransport;
use super::{paused, Channel, Transport};
use crate::config::Multipath as MpCfg;
use link::TcpLink;
use planner::LinkKind;
use scheduler::Scheduler;

pub use planner::parse_lan_ip;

pub struct MultipathTransport {
    adb: AdbTransport,
    session: agent::AgentSession,
    kinds: Vec<LinkKind>,
    token: [u8; TOKEN_LEN],
    chunk: u64,
}

impl MultipathTransport {
    /// 探测并启动 agent。
    ///
    /// 返回 `Err` 表示当前环境跑不了多通道（没 adb、没 agent 二进制、只有一条链路），
    /// 调用方据此回落到 `adb push` / MTP —— 这条路径决定"变慢"永远不可能发生。
    pub fn connect(adb: AdbTransport, cfg: &MpCfg, remote_dir: &str) -> Result<Self> {
        let adb_path = adb.adb_path().to_path_buf();
        let serial = adb.serial().map(str::to_string);

        // 前置预检：手机没连 WiFi 就一定只有一条链路，此时连 agent 都不必投放
        // （省一次 440KB push + chmod + 探测，避免"开了开关反而变慢"）。
        if !cfg.allow_single_link {
            let out = crate::transport::adb::run(
                &adb_path,
                &["shell", "ip -4 addr show wlan0"],
                serial.as_deref(),
            )
            .unwrap_or_default();
            if planner::parse_lan_ip(&out).is_none() {
                return Err(anyhow!("手机未连 WiFi（wlan0 无私有地址），跳过 agent 投放"));
            }
        }

        let abi = agent::device_abi(&adb_path, serial.as_deref()).unwrap_or_default();
        let binary = agent::find_binary(&cfg.agent_path, &abi).ok_or_else(|| {
            anyhow!(
                "未找到 agent 二进制（期望 plugins/agent/wsync-agent-{abi}）"
            )
        })?;

        let token = new_token();
        let port = cfg.port;
        let session = agent::AgentSession::launch(
            &adb_path,
            serial.as_deref(),
            &binary,
            port,
            &token,
            remote_dir,
        )?;

        let probe = planner::Probe {
            agent_ready: true,
            adb_ok: true,
            lan_addr: session.lan_addr().map(str::to_string),
        };
        let kinds = planner::plan(&probe);
        if !planner::usable(&kinds, cfg.allow_single_link) {
            return Err(anyhow!(
                "只有 {} 条可用链路，多通道没有收益",
                kinds.len()
            ));
        }

        let chunk = (cfg.chunk_kb.max(64) as u64) * 1024;
        Ok(Self {
            adb,
            session,
            kinds,
            token,
            chunk,
        })
    }

    pub fn link_labels(&self) -> Vec<String> {
        self.kinds.iter().map(|k| k.label()).collect()
    }

    /// 按 kinds 建立连接并握手。返回可用链路（可能少于 kinds）。
    fn open_links(
        &self,
        total: u64,
        remote_name: &str,
        remote_dir: &str,
    ) -> Result<Vec<TcpLink>> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for kind in &self.kinds {
            let endpoint = kind.endpoint(self.session.port());
            let r = TcpLink::connect(kind.label(), &endpoint)
                .and_then(|mut l| {
                    l.hello(&self.token, total, remote_name, remote_dir)?;
                    Ok(l)
                });
            match r {
                Ok(l) => out.push(l),
                Err(e) => errors.push(format!("{endpoint}: {e}")),
            }
        }
        if out.is_empty() {
            return Err(anyhow!("所有链路都连不上: {}", errors.join("; ")));
        }
        Ok(out)
    }
}

struct WorkerOut {
    link: Option<TcpLink>,
    err: Option<String>,
}

fn hash_local(p: &Path) -> Result<[u8; 32]> {
    let mut f = File::open(p).with_context(|| format!("打开 {} 失败", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

/// 单个链路的发送线程：取块 → 读本地 → 发送 → 记账，发不出去就把块还回队列。
fn worker(
    mut link: TcpLink,
    sched: Arc<Scheduler>,
    file: PathBuf,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<WorkerOut>,
) {
    let mut out = WorkerOut { link: None, err: None };
    let opened = File::open(&file);
    let Ok(f) = opened else {
        out.err = Some(format!("打开 {} 失败", file.display()));
        let _ = tx.send(out);
        return;
    };
    let chunk = sched.chunk_size() as usize;
    let mut buf = vec![0u8; chunk];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(ch) = sched.take() else { break };
        let data = &mut buf[..ch.len as usize];
        if let Err(e) = link::read_at(&f, data, ch.offset) {
            sched.requeue(ch.index);
            out.err = Some(format!("读本地文件失败: {e}"));
            break;
        }
        match link.send_chunk(&ch, data) {
            Ok(()) => sched.ack(ch.len),
            Err(e) => {
                sched.requeue(ch.index);
                out.err = Some(format!("{e}"));
                break;
            }
        }
    }
    if out.err.is_none() {
        out.link = Some(link);
    }
    let _ = tx.send(out);
}

/// 并行发送：把 `total` 字节切块，交给 `links` 各自抢发，最后统一校验收尾。
///
/// 与 `Transport` 解耦，是为了能在没有 adb 的机器上直接测（本机起 agent + 两条回环链路）。
pub(crate) fn transfer(
    mut links: Vec<TcpLink>,
    local: &Path,
    total: u64,
    sha: [u8; 32],
    chunk: u64,
    abort: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let sched = Arc::new(Scheduler::new(total, chunk));
    let stop = Arc::new(AtomicBool::new(false));
    let n = links.len();
    let (tx, rx) = mpsc::channel::<WorkerOut>();
    for l in links.drain(..) {
        let sched = sched.clone();
        let stop = stop.clone();
        let file = local.to_path_buf();
        let tx = tx.clone();
        std::thread::spawn(move || worker(l, sched, file, stop, tx));
    }
    drop(tx);

    let mut alive: Vec<TcpLink> = Vec::new();
    let mut finished = 0usize;
    let mut last_err: Option<String> = None;
    while finished < n {
        if abort.load(Ordering::Relaxed) {
            stop.store(true, Ordering::SeqCst);
        }
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(out) => {
                finished += 1;
                if let Some(l) = out.link {
                    alive.push(l);
                }
                if let Some(e) = out.err {
                    last_err = Some(e);
                }
                let (done, total) = sched.progress();
                progress(done, total);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let (done, total) = sched.progress();
                progress(done, total);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if abort.load(Ordering::Relaxed) {
        for l in alive.iter_mut() {
            l.bye();
        }
        return Err(paused());
    }

    if !sched.is_complete() {
        for l in alive.iter_mut() {
            l.bye();
        }
        return Err(anyhow!(
            "多通道传输未完成（{}/{} 字节）: {}",
            sched.progress().0,
            total,
            last_err.unwrap_or_else(|| "链路中断".into())
        ));
    }

    // 所有块都被 ACK 之后才收尾：由任意一条存活链路代表全组发 DONE。
    let mut ok = false;
    let mut done_err: Option<anyhow::Error> = None;
    for l in alive.iter_mut() {
        match l.done(total, sha) {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => done_err = Some(e),
        }
    }
    for l in alive.iter_mut() {
        l.bye();
    }
    if !ok {
        return Err(done_err.unwrap_or_else(|| anyhow!("没有可用链路完成校验")));
    }
    progress(total, total);
    Ok(())
}

impl Transport for MultipathTransport {
    fn channel(&self) -> Channel {
        Channel::Multipath
    }

    fn device_name(&self) -> String {
        let base = self.adb.device_name();
        let links = self.link_labels().join(" + ");
        format!("{base} [{links}]")
    }

    fn storages(&self) -> Vec<String> {
        self.adb.storages()
    }

    fn ensure_dir(&mut self, remote_dir: &str) -> Result<()> {
        self.adb.ensure_dir(remote_dir)
    }

    fn ping(&mut self) -> Result<()> {
        self.adb.ping()
    }

    fn list_files(&mut self, remote_dir: &str) -> Result<Vec<(String, u64)>> {
        self.adb.list_files(remote_dir)
    }

    fn upload(
        &mut self,
        local: &Path,
        remote_dir: &str,
        remote_name: &str,
        abort: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()> {
        let total = std::fs::metadata(local)
            .map(|m| m.len())
            .with_context(|| format!("读取 {} 元数据失败", local.display()))?;
        progress(0, total);

        let sha = hash_local(local)?;
        let conns = self.open_links(total, remote_name, remote_dir)?;
        transfer(conns, local, total, sha, self.chunk, abort, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_proto_oneshot() {
        let p = std::env::temp_dir().join(format!("wsync-hash-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert_eq!(hash_local(&p).unwrap(), wsync_proto::sha256(&data));
        let _ = std::fs::remove_file(&p);
    }
}
