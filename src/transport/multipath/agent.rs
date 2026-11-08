//! agent 的投放、启动与清理。
//!
//! 绿色约定：二进制只存在于 `/data/local/tmp`，进程只活到传输结束。
//! 每次启动前先删旧的、结束后 kill + rm，失败路径也尽力清理。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::transport::adb::run;
use crate::util::proc::hide_window;

pub const REMOTE_BIN: &str = "/data/local/tmp/wsync-agent";

fn hex(token: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in token {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Android 的 ABI 名与 Rust target 名不是一套（`arm64-v8a` vs `aarch64`），
/// 文件名按 Rust 习惯命名，这里做一次归一化。
pub fn abi_suffix(abi: &str) -> String {
    match abi.trim() {
        "arm64-v8a" => "aarch64".into(),
        "armeabi-v7a" | "armeabi" => "armv7".into(),
        "x86" => "i686".into(),
        other => other.to_string(),
    }
}

/// 按 ABI 找本机的 agent 二进制。
///
/// 查找顺序：显式配置 → `plugins/agent/wsync-agent-<arch>` → `plugins/agent/wsync-agent`。
pub fn find_binary(explicit: &str, abi: &str) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if explicit.trim().is_empty() {
        let dir = crate::util::app_dir().join("plugins").join("agent");
        let arch = abi_suffix(abi);
        // 不放 .exe 候选：手机端只能执行与本机 ABI 匹配的 ELF，
        // 误选一个 Windows PE 过去只会多一次必然失败的投放。
        let mut list = vec![dir.join(format!("wsync-agent-{arch}"))];
        if abi.trim() != arch {
            // 兼容直接用 ABI 原名命名的文件（wsync-agent-arm64-v8a）
            list.push(dir.join(format!("wsync-agent-{}", abi.trim())));
        }
        list.push(dir.join("wsync-agent"));
        list
    } else {
        vec![PathBuf::from(explicit.trim())]
    };
    candidates.into_iter().find(|p| p.is_file())
}

pub fn device_abi(adb: &Path, serial: Option<&str>) -> Option<String> {
    let out = run(adb, &["shell", "getprop", "ro.product.cpu.abi"], serial).ok()?;
    let abi = out.trim().to_string();
    (!abi.is_empty()).then_some(abi)
}

pub struct AgentSession {
    adb: PathBuf,
    serial: Option<String>,
    port: u16,
    child: Option<Child>,
    /// 手机在局域网的 `ip:port`（探测不到为 None）
    lan_addr: Option<String>,
}

impl AgentSession {
    /// 投放并启动 agent，返回可用会话。
    ///
    /// `dir` 是手机侧接收目录（POSIX 路径）。
    pub fn launch(
        adb: &Path,
        serial: Option<&str>,
        binary: &Path,
        port: u16,
        token: &[u8; 32],
        dir: &str,
    ) -> Result<Self> {
        // 1) 旧进程/旧文件先清掉，避免"明明重启了却还连着上一版"的错觉
        let _ = run(adb, &["shell", format!("pkill -f {REMOTE_BIN}").as_str()], serial);
        let _ = run(adb, &["shell", format!("rm -f {REMOTE_BIN}").as_str()], serial);

        // 2) 投放
        let local = binary.to_string_lossy().to_string();
        run(adb, &["push", &local, REMOTE_BIN], serial)
            .with_context(|| format!("投放 agent 失败: {}", binary.display()))?;
        run(adb, &["shell", format!("chmod 755 {REMOTE_BIN}").as_str()], serial)
            .context("chmod agent 失败（/data/local/tmp 可能不可执行）")?;
        // 探一下真能跑：某些机型 mount 了 noexec
        let probe = run(adb, &["shell", format!("{REMOTE_BIN} --help").as_str()], serial)
            .context("agent 在当前设备上无法执行")?;
        if !probe.contains("wsync-agent") {
            return Err(anyhow!("agent 启动探测失败（noexec?）"));
        }

        // 3) 局域网地址：优先 wlan0，供第二条链路直连
        let lan_addr = run(adb, &["shell", "ip -4 addr show wlan0"], serial)
            .ok()
            .and_then(|out| super::planner::parse_lan_ip(&out))
            .map(|ip| format!("{ip}:{port}"))
            .filter(|addr| {
                addr.parse::<SocketAddr>()
                    .map(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(400)).is_ok())
                    .unwrap_or(false)
            });

        // 4) USB 侧端口映射
        run(adb, &["forward", &format!("tcp:{port}"), &format!("tcp:{port}")], serial)
            .context("adb forward 失败")?;

        // 5) 常驻启动：父进程是本机 adb 客户端，Drop 时 kill
        let mut cmd = Command::new(adb);
        if let Some(s) = serial {
            cmd.args(["-s", s]);
        }
        cmd.arg("shell")
            .arg(REMOTE_BIN)
            .args(["--token", &hex(token)])
            .args(["--port", &port.to_string()])
            .args(["--dir", dir])
            .args(["--idle", "20"])
            .args(["--quiet"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_window(&mut cmd);
        let child = cmd.spawn().context("启动 agent 进程失败")?;

        let session = Self {
            adb: adb.to_path_buf(),
            serial: serial.map(str::to_string),
            port,
            child: Some(child),
            lan_addr,
        };
        session.wait_ready()?;
        Ok(session)
    }

    /// 等 USB 侧端口可连（agent 启动 + forward 生效需要一点时间）。
    fn wait_ready(&self) -> Result<()> {
        let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
        let mut last = String::from("未尝试");
        for _ in 0..40 {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                Ok(s) => {
                    drop(s);
                    return Ok(());
                }
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(anyhow!("agent 未就绪: {last}"))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn lan_addr(&self) -> Option<&str> {
        self.lan_addr.as_deref()
    }

    pub fn cleanup(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let serial = self.serial.as_deref();
        let _ = run(
            &self.adb,
            &["shell", format!("pkill -f {REMOTE_BIN}").as_str()],
            serial,
        );
        let _ = run(&self.adb, &["shell", format!("rm -f {REMOTE_BIN}").as_str()], serial);
        let _ = run(
            &self.adb,
            &["forward", "--remove", format!("tcp:{}", self.port).as_str()],
            serial,
        );
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.cleanup();
    }
}


/// 本地自测用：直接跑本机的 agent（不经过 adb）。
#[cfg(test)]
pub fn local_launch(binary: &Path, port: u16, token: &[u8; 32], dir: &str) -> Result<Child> {
    let mut cmd = Command::new(binary);
    cmd.args(["--token", &hex(token)])
        .args(["--port", &port.to_string()])
        .args(["--bind", "127.0.0.1"])
        .args(["--dir", dir])
        .args(["--idle", "20"])
        .args(["--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    hide_window(&mut cmd);
    cmd.spawn().context("启动本地 agent 失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_android_abi_to_artifact_name() {
        assert_eq!(abi_suffix("arm64-v8a"), "aarch64");
        assert_eq!(abi_suffix("armeabi-v7a"), "armv7");
        assert_eq!(abi_suffix("x86_64"), "x86_64");
        assert_eq!(abi_suffix("x86"), "i686");
        assert_eq!(abi_suffix(""), "");
    }
}
