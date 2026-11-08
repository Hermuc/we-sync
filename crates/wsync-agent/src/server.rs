//! 会话层：accept 多连接、握手校验、按 offset 落盘、空闲自退。

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wsync_proto::{read_msg, write_msg, Msg, VERSION};

use crate::store::{Entry, Store};

pub struct Config {
    pub token: [u8; 32],
    /// 允许写入的根目录：只认启动参数，不认客户端在 HELLO 里报的路径
    pub dir: String,
    pub chunk: u32,
    pub once: bool,
    pub quiet: bool,
}

pub struct Server {
    pub cfg: Config,
    pub store: Store,
    active: AtomicUsize,
    finished: AtomicBool,
    last_active: Mutex<Instant>,
}

impl Server {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            store: Store::new(),
            active: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            last_active: Mutex::new(Instant::now()),
        })
    }

    fn touch(&self) {
        *self.last_active.lock().unwrap() = Instant::now();
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::SeqCst);
        self.touch();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn idle_for(&self) -> Duration {
        self.last_active.lock().unwrap().elapsed()
    }

    /// 轮询式 accept：便于在「无连接超过 idle」时自行退出。
    pub fn run(self: &Arc<Self>, listener: TcpListener, idle: Duration) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // listener 是 non-blocking 的，accept 出来的 socket 会继承这个属性；
                    // 会话里用的是阻塞式 read_exact/write_all，必须显式切回阻塞。
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let srv = self.clone();
                    self.active.fetch_add(1, Ordering::SeqCst);
                    self.touch();
                    std::thread::spawn(move || {
                        srv.session(stream);
                        srv.active.fetch_sub(1, Ordering::SeqCst);
                        srv.touch();
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if self.cfg.once && self.is_finished() && self.active.load(Ordering::SeqCst) == 0
                    {
                        return Ok(());
                    }
                    if !self.cfg.once
                        && self.active.load(Ordering::SeqCst) == 0
                        && self.idle_for() > idle
                    {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn session(&self, mut stream: TcpStream) {
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        if !self.cfg.quiet {
            println!("[agent] 连接来自 {peer}");
        }
        let r = self.handle(&mut stream);
        match r {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            Err(e) => eprintln!("[agent] 会话结束: {e}"),
        }
    }

    fn handle(&self, stream: &mut TcpStream) -> io::Result<()> {
        let mut entry: Option<Arc<Entry>> = None;
        loop {
            let msg = read_msg(stream)?;
            match msg {
                Msg::Hello {
                    version,
                    token,
                    total,
                    name,
                    // 客户端报的目录一律不信，只用启动参数里的 --dir（防路径穿越）
                    dir: _,
                } => {
                    if version != VERSION {
                        let _ = write_msg(
                            stream,
                            &Msg::DoneErr {
                                msg: format!("协议版本不匹配: agent={VERSION} pc={version}"),
                            },
                        );
                        return Ok(());
                    }
                    if token != self.cfg.token {
                        let _ = write_msg(
                            stream,
                            &Msg::DoneErr {
                                msg: "token 校验失败".into(),
                            },
                        );
                        return Ok(());
                    }
                    let e = self.store.open(&self.cfg.dir, &name, total)?;
                    if !self.cfg.quiet {
                        println!(
                            "[agent] 开始接收 {} ({} B) id={}",
                            e.name, e.total, e.id
                        );
                    }
                    write_msg(
                        stream,
                        &Msg::HelloOk {
                            file_id: e.id,
                            chunk_size: self.cfg.chunk,
                        },
                    )?;
                    entry = Some(e);
                }
                Msg::Data { offset, bytes } => {
                    let Some(e) = entry.as_ref() else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "未握手就发数据",
                        ));
                    };
                    e.write(offset, &bytes)?;
                    write_msg(
                        stream,
                        &Msg::Ack {
                            offset,
                            len: bytes.len() as u32,
                        },
                    )?;
                }
                Msg::Done { total, sha256 } => {
                    let Some(e) = entry.as_ref() else {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "未握手就收尾"));
                    };
                    if total != e.total {
                        write_msg(
                            stream,
                            &Msg::DoneErr {
                                msg: format!("长度不一致: 声明 {total} 实际 {}", e.total),
                            },
                        )?;
                        return Ok(());
                    }
                    match e.finalize(&sha256) {
                        Ok(()) => {
                            if !self.cfg.quiet {
                                println!("[agent] 完成 {} -> {}", e.name, e.dest.display());
                            }
                            let _ = write_msg(stream, &Msg::DoneOk);
                            self.mark_finished();
                            return Ok(());
                        }
                        Err(m) => {
                            eprintln!("[agent] 校验失败: {m}");
                            let _ = write_msg(stream, &Msg::DoneErr { msg: m });
                            return Ok(());
                        }
                    }
                }
                Msg::Bye => return Ok(()),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("意外的帧: {other:?}"),
                    ))
                }
            }
        }
    }
}
