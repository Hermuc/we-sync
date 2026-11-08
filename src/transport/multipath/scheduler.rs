//! 分块调度：全局块号发号器 + 失败重派队列 + 进度统计。
//!
//! 这一层刻意不碰任何 IO 与平台 API —— 传输跑偏时序的 bug 有一半出在调度，
//! 保持纯逻辑才能在普通 `cargo test` 里把它们钉死。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub index: u64,
    pub offset: u64,
    pub len: u32,
}

pub struct Scheduler {
    total: u64,
    chunk: u64,
    count: u64,
    next: AtomicU64,
    retry: Mutex<VecDeque<u64>>,
    done_bytes: AtomicU64,
    taken: AtomicU64,
}

impl Scheduler {
    pub fn new(total: u64, chunk: u64) -> Self {
        let chunk = chunk.max(1);
        let count = total.div_ceil(chunk);
        Self {
            total,
            chunk,
            count,
            next: AtomicU64::new(0),
            retry: Mutex::new(VecDeque::new()),
            done_bytes: AtomicU64::new(0),
            taken: AtomicU64::new(0),
        }
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk
    }

    /// 取下一块：优先重派队列，其次取新号。返回 `None` 表示全部发完。
    pub fn take(&self) -> Option<Chunk> {
        let index = {
            let mut q = self.retry.lock().unwrap();
            q.pop_front()
        }
        .or_else(|| {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            (i < self.count).then_some(i)
        })?;
        self.taken.fetch_add(1, Ordering::SeqCst);
        let offset = index * self.chunk;
        let len = (self.total - offset).min(self.chunk) as u32;
        Some(Chunk { index, offset, len })
    }

    /// 链路中途死掉时把已取走的块放回队列，交给其它链路。
    pub fn requeue(&self, index: u64) {
        self.taken.fetch_sub(1, Ordering::SeqCst);
        self.retry.lock().unwrap().push_back(index);
    }

    /// 一块真正写完（收到对端 ACK）后记账。
    pub fn ack(&self, len: u32) {
        self.done_bytes.fetch_add(len as u64, Ordering::SeqCst);
    }

    pub fn progress(&self) -> (u64, u64) {
        (self.done_bytes.load(Ordering::SeqCst), self.total)
    }

    pub fn is_complete(&self) -> bool {
        self.done_bytes.load(Ordering::SeqCst) >= self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_all_chunks_including_tail() {
        let s = Scheduler::new(2_500, 1_000);
        let mut got = Vec::new();
        while let Some(c) = s.take() {
            got.push(c);
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].offset, 0);
        assert_eq!(got[0].len, 1_000);
        assert_eq!(got[2].offset, 2_000);
        assert_eq!(got[2].len, 500, "尾块要按剩余长度收敛");
        assert!(s.take().is_none());
    }

    #[test]
    fn requeued_chunk_is_handed_out_again() {
        let s = Scheduler::new(3_000, 1_000);
        let a = s.take().unwrap();
        let b = s.take().unwrap();
        s.requeue(a.index);
        let again = s.take().unwrap();
        assert_eq!(again.index, a.index, "重派的块应优先发出");
        assert_eq!(s.take().unwrap().index, b.index + 1);
    }

    #[test]
    fn progress_and_completion() {
        let s = Scheduler::new(2_000, 1_000);
        assert_eq!(s.progress(), (0, 2_000));
        assert!(!s.is_complete());
        for _ in 0..2 {
            let c = s.take().unwrap();
            s.ack(c.len);
        }
        assert!(s.is_complete());
        assert_eq!(s.progress(), (2_000, 2_000));
    }

    #[test]
    fn empty_file_completes_immediately() {
        let s = Scheduler::new(0, 1_000);
        assert!(s.take().is_none());
        assert!(s.is_complete());
    }
}
