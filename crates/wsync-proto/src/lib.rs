//! WE-Sync 多通道传输协议：帧编解码 + 校验和。
//!
//! PC 端与 Android agent 共用同一份实现，避免两份协议各自漂移。
//! 全程零外部依赖：agent 要交叉编译到 Android，体积与构建面都要压到最小。

pub mod frame;
pub mod hash;

pub use frame::{read_msg, write_data_frame, write_msg, Msg, MAGIC, MAX_PAYLOAD, TOKEN_LEN, VERSION};
pub use hash::{crc32, sha256, Sha256};

/// 默认分块大小：1MB。太小调度开销占比高，太大尾块等待会拖慢结束。
pub const DEFAULT_CHUNK: u32 = 1024 * 1024;

/// 生成 32 字节一次性会话 token。
///
/// 用标准库 `RandomState`（内部由操作系统随机种子初始化）拼 4 轮哈希，
/// 不引入 rand 依赖；token 只保护一次几秒的传输，且只在 adb 探测到的私有地址上监听。
pub fn new_token() -> [u8; TOKEN_LEN] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let rs = RandomState::new();
    let mut out = [0u8; TOKEN_LEN];
    for i in 0..4u8 {
        let mut h = rs.build_hasher();
        h.write_u8(i);
        h.write_u128(nanos);
        h.write_u128(pid);
        h.write_usize(&out as *const _ as usize);
        out[i as usize * 8..i as usize * 8 + 8].copy_from_slice(&h.finish().to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_not_constant() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_LEN);
    }
}
