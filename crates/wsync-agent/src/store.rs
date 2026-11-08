//! 落盘侧：登记表、按 offset 写入、整文件校验后改名。
//!
//! 写入一律先落到 `<name>.wsync` 临时文件，校验通过才 rename ——
//! 中途失败（拔线 / PC 崩溃）不会在目标目录留下半个壁纸。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wsync_proto::Sha256;

pub fn tmp_suffix() -> &'static str {
    ".wsync"
}

/// 平台无关的按偏移写入（Windows `seek_write` / Unix `write_at`）。
fn write_at(file: &File, buf: &[u8], off: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let n = file.seek_write(&buf[done..], off + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "写入 0 字节"));
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let n = file.write_at(&buf[done..], off + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "写入 0 字节"));
            }
            done += n;
        }
        Ok(())
    }
}

pub struct Entry {
    pub id: u32,
    pub name: String,
    pub tmp: PathBuf,
    pub dest: PathBuf,
    pub total: u64,
    pub file: File,
    pub finalized: AtomicBool,
}

pub struct Store {
    next_id: Mutex<u32>,
    by_key: Mutex<HashMap<String, Arc<Entry>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            next_id: Mutex::new(1),
            by_key: Mutex::new(HashMap::new()),
        }
    }

    /// 启动时清掉上次异常退出留下的半截文件（只碰自己的后缀）。
    pub fn clean_stale(dir: &Path) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.to_string_lossy().ends_with(tmp_suffix()) {
                let _ = fs::remove_file(&p);
            }
        }
    }

    pub fn open(&self, dir: &str, name: &str, total: u64) -> io::Result<Arc<Entry>> {
        let key = format!("{dir}\u{0}{name}\u{0}{total}");
        if let Some(e) = self.by_key.lock().unwrap().get(&key) {
            return Ok(e.clone());
        }

        let dest = Path::new(dir).join(name);
        // 先写 <name>.wsync，校验通过才改名 —— 中途失败不会留半个壁纸
        let tmp = Path::new(dir).join(format!("{name}{}", tmp_suffix()));
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&tmp)?;
        file.set_len(total)?;

        let mut id_guard = self.next_id.lock().unwrap();
        let id = *id_guard;
        *id_guard += 1;
        drop(id_guard);

        let entry = Arc::new(Entry {
            id,
            name: name.to_string(),
            tmp,
            dest,
            total,
            file,
            finalized: AtomicBool::new(false),
        });
        self.by_key.lock().unwrap().insert(key, entry.clone());
        Ok(entry)
    }
}

impl Entry {
    pub fn write(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "写入越界（offset + len > total）",
            ));
        }
        write_at(&self.file, buf, offset)
    }

    /// 校验整文件 SHA-256，通过才改名。返回实际哈希（失败时用于日志）。
    pub fn finalize(&self, want: &[u8; 32]) -> Result<(), String> {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.file.sync_all();

        let got = hash_file(&self.tmp).map_err(|e| format!("读取临时文件失败: {e}"))?;
        if &got != want {
            let _ = fs::remove_file(&self.tmp);
            return Err(format!(
                "SHA-256 不一致: 期望 {} 实际 {}",
                hex(want),
                hex(&got)
            ));
        }
        let _ = fs::remove_file(&self.dest);
        fs::rename(&self.tmp, &self.dest).map_err(|e| format!("改名失败: {e}"))?;
        Ok(())
    }
}

pub fn hash_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(p)?;
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

fn hex(d: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

