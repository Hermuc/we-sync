//! WE-Sync 手机端接收器。
//!
//! 设计前提：**不安装、不留痕**。由 PC 端用 `adb push` 投放到
//! `/data/local/tmp/wsync-agent` 直接执行，传输结束后进程退出、文件被删除。
//! 因此这里不能有任何平台专属依赖，也不能假设自己被装成 Android App。
//!
//! 交叉编译（在有 NDK 的机器上）：
//! ```text
//! rustup target add aarch64-linux-android
//! cargo ndk -t arm64-v8a build --release -p wsync-agent
//! 产物 -> plugins/agent/wsync-agent-aarch64
//! ```

mod cli;
mod server;
mod store;

use std::net::TcpListener;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use server::{Config, Server};

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("参数错误: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    let dir = Path::new(&args.dir);
    if args.clean {
        store::Store::clean_stale(dir);
    }

    let listener = match TcpListener::bind((args.bind.as_str(), args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {}:{} 失败: {e}", args.bind, args.port);
            return ExitCode::from(1);
        }
    };

    if !args.quiet {
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        println!("[agent] 监听 {local}，目录 {}", dir.display());
    }

    let srv = Server::new(Config {
        token: args.token,
        dir: dir.to_string_lossy().to_string(),
        chunk: args.chunk,
        once: args.once,
        quiet: args.quiet,
    });

    match srv.run(listener, Duration::from_secs(args.idle_secs)) {
        Ok(()) => {
            if !args.quiet {
                println!("[agent] 退出");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[agent] 监听异常: {e}");
            ExitCode::from(1)
        }
    }
}
