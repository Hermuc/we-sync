//! adb 通道：要求手机开启「USB 调试」并授权。速度快，实现简单。

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{Channel, Transport};

pub struct AdbTransport {
    adb: PathBuf,
    serial: Option<String>,
    pub storages: Vec<String>,
}

impl AdbTransport {
    /// 若存在已授权设备则返回可用通道。
    pub fn detect(adb: PathBuf) -> Option<Self> {
        let out = run(&adb, &["devices"], None).ok()?;
        let serial = out
            .lines()
            .skip(1)
            .find_map(|l| {
                let mut it = l.split_whitespace();
                let id = it.next()?;
                let state = it.next()?;
                (state == "device").then(|| id.to_string())
            })?;
        // adb devices 里可能出现"幽灵设备"（拔插后 daemon 未刷新），实测一次再采用
        let probe = run(&adb, &["shell", "echo", "ok"], Some(&serial)).ok()?;
        if !probe.contains("ok") {
            return None;
        }
        Some(Self {
            adb,
            serial: Some(serial),
            storages: vec!["/sdcard".into()],
        })
    }

    fn shell(&self, args: &[&str]) -> Result<String> {
        let mut full: Vec<String> = vec!["shell".into()];
        for a in args {
            full.push((*a).to_string());
        }
        let refs: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        run(&self.adb, &refs, self.serial.as_deref())
    }

    fn quote(p: &str) -> String {
        // 用单引号包裹，内部单引号转义，避免 shell 解析空格
        format!("'{}'", p.replace('\'', r"'\''"))
    }

    /// adb.exe 路径（多通道层复用它投放 agent）。
    pub fn adb_path(&self) -> &Path {
        &self.adb
    }

    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

impl Transport for AdbTransport {
    fn channel(&self) -> Channel {
        Channel::Adb
    }

    fn device_name(&self) -> String {
        let model = self
            .shell(&["getprop", "ro.product.model"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if model.is_empty() {
            self.serial.clone().unwrap_or_else(|| "android".into())
        } else {
            format!("{model} (adb)")
        }
    }

    fn storages(&self) -> Vec<String> {
        self.storages.clone()
    }

    fn ping(&mut self) -> Result<()> {
        // `get-state` 比 `devices` 便宜，而且能区分 device / offline / unauthorized：
        // 拔线时 adb 会直接以非 0 退出码结束（device not found），同样走到 Err。
        let out = run(&self.adb, &["get-state"], self.serial.as_deref())?;
        let st = out.trim();
        if st == "device" {
            Ok(())
        } else {
            Err(anyhow!("adb 设备状态 {st}"))
        }
    }

    fn ensure_dir(&mut self, remote_dir: &str) -> Result<()> {
        let cmd = format!("mkdir -p {}", Self::quote(remote_dir));
        let out = self.shell(&[cmd.as_str()])?;
        let low = out.to_lowercase();
        if low.contains("denied") || low.contains("error") {
            return Err(anyhow!("创建目录失败: {}", out.trim()));
        }
        Ok(())
    }

    fn list_files(&mut self, remote_dir: &str) -> Result<Vec<(String, u64)>> {
        let cmd = format!(
            "for f in {}/*; do [ -f \"$f\" ] && echo \"$(stat -c %s \"$f\") $(basename \"$f\")\"; done",
            Self::quote(remote_dir)
        );
        let out = match self.shell(&[cmd.as_str()]) {
            Ok(o) => o,
            Err(_) => return Ok(Vec::new()),
        };
        let mut v = Vec::new();
        for line in out.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(2, ' ');
            let (Some(size), Some(name)) = (it.next(), it.next()) else {
                continue;
            };
            if let Ok(size) = size.trim().parse::<u64>() {
                v.push((name.trim().to_string(), size));
            }
        }
        Ok(v)
    }

    fn upload(
        &mut self,
        local: &Path,
        remote_dir: &str,
        remote_name: &str,
        abort: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()> {
        let dest = format!("{remote_dir}/{remote_name}");
        let total = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
        progress(0, total);

        let mut cmd = Command::new(&self.adb);
        if let Some(s) = &self.serial {
            cmd.args(["-s", s]);
        }
        cmd.arg("push").arg(local).arg(&dest);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        crate::util::proc::hide_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| "启动 adb push 失败")?;

        // 轮询等待，可随时 kill 掉子进程实现"立即暂停"
        let status = loop {
            if abort.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                let cleanup = format!("rm -f {}", Self::quote(&dest));
                let _ = self.shell(&[cleanup.as_str()]);
                return Err(super::paused());
            }
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => std::thread::sleep(Duration::from_millis(120)),
                Err(e) => return Err(anyhow!("等待 adb push 失败: {e}")),
            }
        };

        let mut stderr = String::new();
        if let Some(mut s) = child.stderr.take() {
            use std::io::Read;
            let _ = s.read_to_string(&mut stderr);
        }
        if !status.success() {
            return Err(anyhow!("adb push 失败: {}", stderr.trim()));
        }
        if stderr.to_lowercase().contains("error") {
            return Err(anyhow!("adb push 报错: {}", stderr.trim()));
        }
        progress(total, total);
        crate::log_debug!("adb push 完成: {} -> {}", local.display(), dest);
        Ok(())
    }
}

pub(crate) fn run(adb: &Path, args: &[&str], serial: Option<&str>) -> Result<String> {
    let mut cmd = Command::new(adb);
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::util::proc::hide_window(&mut cmd);
    let out = cmd.output().with_context(|| format!("执行 adb 失败: {}", adb.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "adb 返回错误: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
