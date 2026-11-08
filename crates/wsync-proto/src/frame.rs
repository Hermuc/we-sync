//! 帧格式与消息编解码。
//!
//! ```text
//! "WSY1" | tag(u8) | len(u32 LE) | payload(len) | crc32(u32 LE)
//! ```
//! CRC 覆盖 `tag + len + payload`，任何一位翻转都会在解码时被拒绝。
//! 手写编解码而不用 serde：agent 要跑在 Android 上，省一个依赖就少一份体积。

use std::io::{self, Read, Write};

use crate::hash::{crc32, crc32_update};

pub const MAGIC: [u8; 4] = *b"WSY1";
pub const VERSION: u16 = 1;
/// 单帧 payload 上限（防对端报一个巨型长度把内存吃光）。
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// HELLO 里 token 的固定长度。
pub const TOKEN_LEN: usize = 32;

const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_OK: u8 = 0x02;
const TAG_DATA: u8 = 0x10;
const TAG_ACK: u8 = 0x11;
const TAG_DONE: u8 = 0x20;
const TAG_DONE_OK: u8 = 0x21;
const TAG_DONE_ERR: u8 = 0x22;
const TAG_BYE: u8 = 0x7F;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// 每条数据链路建立后先握手；同名同总长的连接共享同一个写入目标。
    Hello {
        version: u16,
        token: [u8; TOKEN_LEN],
        total: u64,
        name: String,
        dir: String,
    },
    HelloOk { file_id: u32, chunk_size: u32 },
    Data { offset: u64, bytes: Vec<u8> },
    Ack { offset: u64, len: u32 },
    /// 全部块写完，带整文件 SHA-256 由 agent 校验落盘结果。
    Done { total: u64, sha256: [u8; 32] },
    DoneOk,
    DoneErr { msg: String },
    Bye,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(u16::MAX as usize);
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&b[..n]);
}

struct Cur<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(bad("帧 payload 截断"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> io::Result<String> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| bad("字符串不是 UTF-8"))
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
}

fn encode_payload(m: &Msg) -> (u8, Vec<u8>) {
    let mut p = Vec::with_capacity(64);
    match m {
        Msg::Hello {
            version,
            token,
            total,
            name,
            dir,
        } => {
            p.extend_from_slice(&version.to_le_bytes());
            p.extend_from_slice(token);
            p.extend_from_slice(&total.to_le_bytes());
            put_str(&mut p, name);
            put_str(&mut p, dir);
            (TAG_HELLO, p)
        }
        Msg::HelloOk {
            file_id,
            chunk_size,
        } => {
            p.extend_from_slice(&file_id.to_le_bytes());
            p.extend_from_slice(&chunk_size.to_le_bytes());
            (TAG_HELLO_OK, p)
        }
        Msg::Data { offset, bytes } => {
            p.extend_from_slice(&offset.to_le_bytes());
            p.extend_from_slice(bytes);
            (TAG_DATA, p)
        }
        Msg::Ack { offset, len } => {
            p.extend_from_slice(&offset.to_le_bytes());
            p.extend_from_slice(&len.to_le_bytes());
            (TAG_ACK, p)
        }
        Msg::Done { total, sha256 } => {
            p.extend_from_slice(&total.to_le_bytes());
            p.extend_from_slice(sha256);
            (TAG_DONE, p)
        }
        Msg::DoneOk => (TAG_DONE_OK, p),
        Msg::DoneErr { msg } => {
            put_str(&mut p, msg);
            (TAG_DONE_ERR, p)
        }
        Msg::Bye => (TAG_BYE, p),
    }
}

fn decode_payload(tag: u8, buf: &[u8]) -> io::Result<Msg> {
    let mut c = Cur { buf, pos: 0 };
    let m = match tag {
        TAG_HELLO => {
            let version = c.u16()?;
            let mut token = [0u8; TOKEN_LEN];
            token.copy_from_slice(c.take(TOKEN_LEN)?);
            let total = c.u64()?;
            let name = c.str()?;
            let dir = c.str()?;
            Msg::Hello {
                version,
                token,
                total,
                name,
                dir,
            }
        }
        TAG_HELLO_OK => Msg::HelloOk {
            file_id: c.u32()?,
            chunk_size: c.u32()?,
        },
        TAG_DATA => Msg::Data {
            offset: c.u64()?,
            bytes: c.rest().to_vec(),
        },
        TAG_ACK => Msg::Ack {
            offset: c.u64()?,
            len: c.u32()?,
        },
        TAG_DONE => {
            let total = c.u64()?;
            let mut sha = [0u8; 32];
            sha.copy_from_slice(c.take(32)?);
            Msg::Done { total, sha256: sha }
        }
        TAG_DONE_OK => Msg::DoneOk,
        TAG_DONE_ERR => Msg::DoneErr { msg: c.str()? },
        TAG_BYE => Msg::Bye,
        other => return Err(bad(format!("未知帧类型 0x{other:02X}"))),
    };
    if c.pos != buf.len() {
        return Err(bad("帧 payload 尾部有多余字节"));
    }
    Ok(m)
}

pub fn write_msg<W: Write>(w: &mut W, m: &Msg) -> io::Result<()> {
    let (tag, payload) = encode_payload(m);
    if payload.len() as u64 > MAX_PAYLOAD as u64 {
        return Err(bad("payload 超过上限"));
    }
    let mut head = Vec::with_capacity(payload.len() + 5);
    head.push(tag);
    head.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    head.extend_from_slice(&payload);
    let crc = crc32(&head);
    w.write_all(&MAGIC)?;
    w.write_all(&head)?;
    w.write_all(&crc.to_le_bytes())?;
    w.flush()
}

/// 专用于 DATA 帧的零拷贝写出。
///
/// 1MB 的块如果先 `to_vec()` 再编码，传输一秒钟就要多拷几百 MB；
/// 这里直接把切片写进 socket，CRC 分三段增量计算。
pub fn write_data_frame<W: Write>(w: &mut W, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let plen = 8usize + bytes.len();
    if plen as u64 > MAX_PAYLOAD as u64 {
        return Err(bad("块超过 payload 上限"));
    }
    let len_bytes = (plen as u32).to_le_bytes();
    let off_bytes = offset.to_le_bytes();
    let mut crc = crc32(&[TAG_DATA]);
    crc = crc32_update(crc, &len_bytes);
    crc = crc32_update(crc, &off_bytes);
    crc = crc32_update(crc, bytes);

    w.write_all(&MAGIC)?;
    w.write_all(&[TAG_DATA])?;
    w.write_all(&len_bytes)?;
    w.write_all(&off_bytes)?;
    w.write_all(bytes)?;
    w.write_all(&crc.to_le_bytes())?;
    w.flush()
}

pub fn read_msg<R: Read>(r: &mut R) -> io::Result<Msg> {    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(bad("magic 不匹配，对端不是 WE-Sync agent"));
    }
    let mut h = [0u8; 5];
    r.read_exact(&mut h)?;
    let tag = h[0];
    let len = u32::from_le_bytes(h[1..5].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(bad(format!("帧长度 {len} 超过上限")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    let mut c = [0u8; 4];
    r.read_exact(&mut c)?;
    let want = u32::from_le_bytes(c);
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&h);
    buf.extend_from_slice(&payload);
    let got = crc32(&buf);
    if got != want {
        return Err(bad(format!("帧 CRC 校验失败: {got:08X} != {want:08X}")));
    }
    decode_payload(tag, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Msg) {
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        let back = read_msg(&mut buf.as_slice()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn roundtrip_all_variants() {
        roundtrip(Msg::Hello {
            version: VERSION,
            token: [7u8; TOKEN_LEN],
            total: 123456789,
            name: "1234567890 雨天.mp4".into(),
            dir: "/sdcard/.WE".into(),
        });
        roundtrip(Msg::HelloOk {
            file_id: 42,
            chunk_size: 1 << 20,
        });
        roundtrip(Msg::Data {
            offset: 4096,
            bytes: vec![1, 2, 3, 4],
        });
        roundtrip(Msg::Ack {
            offset: 4096,
            len: 4,
        });
        roundtrip(Msg::Done {
            total: 99,
            sha256: [9u8; 32],
        });
        roundtrip(Msg::DoneOk);
        roundtrip(Msg::DoneErr {
            msg: "hash mismatch".into(),
        });
        roundtrip(Msg::Bye);
    }

    #[test]
    fn zero_copy_data_frame_roundtrips() {
        let payload = vec![0xA5u8; 4096];
        let mut buf = Vec::new();
        write_data_frame(&mut buf, 8192, &payload).unwrap();
        match read_msg(&mut buf.as_slice()).unwrap() {
            Msg::Data { offset, bytes } => {
                assert_eq!(offset, 8192);
                assert_eq!(bytes, payload);
            }
            other => panic!("期望 Data，得到 {other:?}"),
        }
    }

    #[test]
    fn rejects_corrupted_frame() {
        let mut buf = Vec::new();
        write_msg(
            &mut buf,
            &Msg::Data {
                offset: 0,
                bytes: vec![1, 2, 3],
            },
        )
        .unwrap();
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        assert!(read_msg(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut buf = b"XXXX\x7f\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        buf.truncate(14);
        assert!(read_msg(&mut buf.as_slice()).is_err());
    }
}
