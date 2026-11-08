//! 端到端自测：在本机启动 agent，用两条 TCP 链路并行送同一个文件。
//!
//! 这条测试就是"Android 只差一次交叉编译"的证据 —— 协议、调度、落盘、
//! 校验、改名全部走真实代码路径，只是链路换成了回环。
//!
//! 前置：`cargo build -p wsync-agent`（找不到二进制时自动跳过，不污染 CI）。

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::agent::local_launch;
use super::link::TcpLink;
use super::transfer;

fn find_agent() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    ["wsync-agent.exe", "wsync-agent"]
        .iter()
        .map(|n| root.join("target").join("debug").join(n))
        .find(|p| p.is_file())
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口失败");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn wait_port(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    for _ in 0..60 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("agent 未在 3 秒内就绪");
}

#[test]
fn two_links_deliver_identical_file() {
    let Some(agent) = find_agent() else {
        eprintln!("跳过端到端测试：先执行 cargo build -p wsync-agent");
        return;
    };

    let base = std::env::temp_dir().join(format!("wsync-e2e-{}", std::process::id()));
    let dst_dir = base.join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();

    // 非 ASCII 文件名：手机上的壁纸名基本都是中文
    let name = "雨天街道 [1234567890].mp4";
    let local = base.join(name);
    let total = 8u64 * 1024 * 1024;
    let data: Vec<u8> = (0..total).map(|i| ((i * 31) % 251) as u8).collect();
    std::fs::write(&local, &data).unwrap();

    let port = free_port();
    let token = wsync_proto::new_token();
    let dst_str = dst_dir.to_string_lossy().to_string();
    let mut child = local_launch(&agent, port, &token, &dst_str).expect("启动 agent 失败");
    wait_port(port);

    let sha = wsync_proto::sha256(&data);
    let mut links = Vec::new();
    for _ in 0..2 {
        let mut l = TcpLink::connect("loop", &format!("127.0.0.1:{port}")).expect("连接 agent 失败");
        l.hello(&token, total, name, &dst_str).expect("HELLO 失败");
        links.push(l);
    }

    let abort = AtomicBool::new(false);
    let mut last = 0u64;
    {
        let mut cb = |done: u64, _total: u64| last = done;
        transfer(links, &local, total, sha, 1024 * 1024, &abort, &mut cb)
            .expect("双链路传输失败");
    }

    assert_eq!(last, total, "进度应走到 100%");
    let out = dst_dir.join(name);
    let got = std::fs::read(&out).expect("目标文件不存在");
    assert_eq!(got.len() as u64, total);
    assert_eq!(got, data, "落盘内容必须与源文件逐字节一致");

    // 临时文件必须已被改名（绿色：不留 .wsync）
    let leftovers: Vec<_> = std::fs::read_dir(&dst_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".wsync"))
        .collect();
    assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn surviving_link_finishes_after_one_breaks() {
    let Some(agent) = find_agent() else {
        eprintln!("跳过端到端测试：先执行 cargo build -p wsync-agent");
        return;
    };
    let base = std::env::temp_dir().join(format!("wsync-break-{}", std::process::id()));
    let dst_dir = base.join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let name = "broken.mp4";
    let local = base.join(name);
    let total = 4u64 * 1024 * 1024;
    let data: Vec<u8> = (0..total).map(|i| (i % 240) as u8).collect();
    std::fs::write(&local, &data).unwrap();

    let port = free_port();
    let token = wsync_proto::new_token();
    let dst_str = dst_dir.to_string_lossy().to_string();
    let mut child = local_launch(&agent, port, &token, &dst_str).expect("启动 agent 失败");
    wait_port(port);

    let mut dead = TcpLink::connect("dead", &format!("127.0.0.1:{port}")).unwrap();
    dead.hello(&token, total, name, &dst_str).unwrap();
    let mut live = TcpLink::connect("live", &format!("127.0.0.1:{port}")).unwrap();
    live.hello(&token, total, name, &dst_str).unwrap();
    // 让第一条链路在开跑前就断掉：它取到的块必须被放回队列，由另一条补上。
    dead.break_stream();

    let abort = AtomicBool::new(false);
    let mut last = 0u64;
    {
        let mut cb = |done: u64, _t: u64| last = done;
        transfer(
            vec![dead, live],
            &local,
            total,
            wsync_proto::sha256(&data),
            1024 * 1024,
            &abort,
            &mut cb,
        )
        .expect("一条链路断掉后，另一条应能独立完成");
    }
    assert_eq!(last, total);
    assert_eq!(std::fs::read(dst_dir.join(name)).unwrap(), data);

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn corrupted_source_is_rejected_by_agent() {
    let Some(agent) = find_agent() else {
        eprintln!("跳过端到端测试：先执行 cargo build -p wsync-agent");
        return;
    };
    let base = std::env::temp_dir().join(format!("wsync-bad-{}", std::process::id()));
    let dst_dir = base.join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let local = base.join("bad.mp4");
    let data = vec![7u8; 2 * 1024 * 1024];
    std::fs::write(&local, &data).unwrap();

    let port = free_port();
    let token = wsync_proto::new_token();
    let dst_str = dst_dir.to_string_lossy().to_string();
    let mut child = local_launch(&agent, port, &token, &dst_str).expect("启动 agent 失败");
    wait_port(port);

    let mut l = TcpLink::connect("loop", &format!("127.0.0.1:{port}")).unwrap();
    l.hello(&token, data.len() as u64, "bad.mp4", &dst_str).unwrap();

    // 故意报一个错误的 SHA-256：agent 必须拒绝并删掉临时文件
    let wrong = [0xABu8; 32];
    let abort = AtomicBool::new(false);
    let mut cb = |_d: u64, _t: u64| {};
    let err = transfer(
        vec![l],
        &local,
        data.len() as u64,
        wrong,
        1024 * 1024,
        &abort,
        &mut cb,
    )
    .expect_err("校验失败时传输必须报错");
    assert!(
        err.to_string().contains("校验"),
        "错误信息应指出校验失败，实际: {err}"
    );
    assert!(!dst_dir.join("bad.mp4").exists(), "校验失败不得产出目标文件");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}
