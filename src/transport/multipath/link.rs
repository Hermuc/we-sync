//! 一条数据链路：TCP 连接 + HELLO/DATA/DONE 收发 + 本地按 offset 读文件。
//!
//! 平台相关的只有 `read_at`（Win32 `seek_read` / POSIX `pread`），其余纯 std。

use std::fs::File;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use wsync_proto::{read_msg, write_data_frame, write_msg, Msg, TOKEN_LEN};

use super::scheduler::Chunk;

/// 握手超时：局域网都不通就别拖着一整轮重试。
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// 单块读写超时：超过即认为链路失活（USB 拥塞/锁屏切 MTP 都会这样）。
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TcpLink {
    pub name: String,
    stream: TcpStream,
}

impl TcpLink {
    pub fn connect(name: impl Into<String>, addr: &str) -> Result<Self> {
        let sa = addr
            .to_socket_addrs()
            .with_context(|| format!("解析地址 {addr} 失败"))?
            .next()
            .ok_or_else(|| anyhow!("地址 {addr} 解析不出 socket"))?;
        let stream = TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT)
            .with_context(|| format!("连接 {addr} 失败"))?;
        let _ = stream.set_nodelay(true);
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self {
            name: name.into(),
            stream,
        })
    }

    pub fn hello(
        &mut self,
        token: &[u8; TOKEN_LEN],
        total: u64,
        file_name: &str,
        dir: &str,
    ) -> Result<()> {
        write_msg(
            &mut self.stream,
            &Msg::Hello {
                version: wsync_proto::VERSION,
                token: *token,
                total,
                name: file_name.to_string(),
                dir: dir.to_string(),
            },
        )
        .with_context(|| format!("{} 发 HELLO 失败", self.name))?;
        match read_msg(&mut self.stream).with_context(|| format!("{} 等 HELLO_OK 失败", self.name))? {
            // 分块大小以本机配置为准，对端建议值只用于协议完整性，不参与调度。
            Msg::HelloOk { .. } => Ok(()),
            Msg::DoneErr { msg } => Err(anyhow!("agent 拒绝连接: {msg}")),
            other => Err(anyhow!("{} 收到意外帧: {other:?}", self.name)),
        }
    }

    /// 发一块并等待 ACK。返回 Err 意味着这条链路不可用了。
    pub fn send_chunk(&mut self, ch: &Chunk, data: &[u8]) -> Result<()> {
        debug_assert_eq!(data.len(), ch.len as usize);
        write_data_frame(&mut self.stream, ch.offset, data)
            .with_context(|| format!("{} 发块 {} 失败", self.name, ch.index))?;
        match read_msg(&mut self.stream) {
            Ok(Msg::Ack { offset, len }) if offset == ch.offset && len == ch.len => Ok(()),
            Ok(Msg::Ack { offset, len }) => Err(anyhow!(
                "{} ACK 不匹配: offset={offset} len={len}，期望 offset={} len={}",
                self.name,
                ch.offset,
                ch.len
            )),
            Ok(other) => Err(anyhow!("{} 等 ACK 收到意外帧: {other:?}", self.name)),
            Err(e) => Err(anyhow!("{} 等 ACK 失败: {e}", self.name)),
        }
    }

    /// 收尾：由一条链路代表全组发 DONE，agent 校验并改名后才算完成。
    pub fn done(&mut self, total: u64, sha256: [u8; 32]) -> Result<()> {
        write_msg(&mut self.stream, &Msg::Done { total, sha256 })
            .with_context(|| format!("{} 发 DONE 失败", self.name))?;
        match read_msg(&mut self.stream) {
            Ok(Msg::DoneOk) => Ok(()),
            Ok(Msg::DoneErr { msg }) => Err(anyhow!("agent 校验失败: {msg}")),
            Ok(other) => Err(anyhow!("{} 收到意外帧: {other:?}", self.name)),
            Err(e) => Err(anyhow!("{} 等 DONE 结果失败: {e}", self.name)),
        }
    }

    pub fn bye(&mut self) {
        let _ = write_msg(&mut self.stream, &Msg::Bye);
    }

    /// 仅测试用：模拟这条链路被拔掉（远端收到 FIN，后续读写立刻报错）。
    #[cfg(test)]
    pub fn break_stream(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// 从文件的 `offset` 处读满 `buf`。
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "文件提前读完"));
            }
            done += n;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let n = file.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "文件提前读完"));
            }
            done += n;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_at_reads_requested_range() {
        let path = std::env::temp_dir().join(format!("wsync-readat-{}.bin", std::process::id()));
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"0123456789abcdef").unwrap();
        }
        let f = File::open(&path).unwrap();
        let mut buf = [0u8; 4];
        read_at(&f, &mut buf, 3).unwrap();
        assert_eq!(&buf, b"3456");
        let mut past = [0u8; 4];
        assert!(read_at(&f, &mut past, 14).is_err(), "越界读应报错");
        let _ = std::fs::remove_file(&path);
    }
}
