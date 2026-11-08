//! 极简参数解析（不引第三方 CLI 库，保持零依赖与可交叉编译）。

use wsync_proto::TOKEN_LEN;

pub struct Args {
    pub bind: String,
    pub port: u16,
    pub dir: String,
    pub token: [u8; TOKEN_LEN],
    pub chunk: u32,
    pub idle_secs: u64,
    pub once: bool,
    pub quiet: bool,
    pub clean: bool,
}

pub const USAGE: &str = "\
wsync-agent — WE-Sync 接收端

用法: wsync-agent --token <hex64> [选项]

  --token <hex>     会话 token（64 位十六进制，PC 端生成后传入）  必需
  --port <n>        监听端口，默认 5740
  --bind <addr>     监听地址，默认 0.0.0.0
  --dir <path>      接收目录，默认当前目录
  --chunk <bytes>   建议分块大小，默认 1048576
  --idle <secs>     无连接多久后自行退出，默认 30（绿色：不留后台进程）
  --once            完成一次传输后立即退出
  --no-clean        启动时不清理目录内残留的 .wsync 半截文件
  --quiet           不打印进度
  -h, --help        显示本帮助
";

fn parse_hex_token(s: &str) -> Result<[u8; TOKEN_LEN], String> {
    let s = s.trim();
    if s.len() != TOKEN_LEN * 2 {
        return Err(format!("token 应为 {} 个十六进制字符", TOKEN_LEN * 2));
    }
    let mut out = [0u8; TOKEN_LEN];
    for i in 0..TOKEN_LEN {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| "token 含非十六进制字符".to_string())?;
    }
    Ok(out)
}

pub fn parse<I: IntoIterator<Item = String>>(iter: I) -> Result<Option<Args>, String> {
    let mut bind = "0.0.0.0".to_string();
    let mut port = 5740u16;
    let mut dir = ".".to_string();
    let mut token: Option<[u8; TOKEN_LEN]> = None;
    let mut chunk = wsync_proto::DEFAULT_CHUNK;
    let mut idle_secs = 30u64;
    let mut once = false;
    let mut quiet = false;
    let mut clean = true;

    let mut it = iter.into_iter().peekable();
    while let Some(a) = it.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (a.clone(), None),
        };
        let mut val = |name: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            it.next()
                .ok_or_else(|| format!("{name} 缺少取值"))
        };
        match key.as_str() {
            "--token" => token = Some(parse_hex_token(&val("--token")?)?),
            "--port" => {
                port = val("--port")?
                    .parse()
                    .map_err(|_| "--port 不是合法端口".to_string())?
            }
            "--bind" => bind = val("--bind")?,
            "--dir" => dir = val("--dir")?,
            "--chunk" => {
                chunk = val("--chunk")?
                    .parse()
                    .map_err(|_| "--chunk 不是合法字节数".to_string())?
            }
            "--idle" => {
                idle_secs = val("--idle")?
                    .parse()
                    .map_err(|_| "--idle 不是合法秒数".to_string())?
            }
            "--once" => once = true,
            "--quiet" => quiet = true,
            "--no-clean" => clean = false,
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("未知参数: {other}")),
        }
    }

    let token = token.ok_or_else(|| "--token 是必需参数".to_string())?;
    Ok(Some(Args {
        bind,
        port,
        dir,
        token,
        chunk,
        idle_secs,
        once,
        quiet,
        clean,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<Args>, String> {
        parse(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_required_token() {
        let a = args(&["--token", &"ab".repeat(32)]).unwrap().unwrap();
        assert_eq!(a.token, [0xab; 32]);
        assert_eq!(a.port, 5740);
    }

    #[test]
    fn accepts_inline_values_and_flags() {
        let a = args(&[
            "--token",
            &"00".repeat(32),
            "--port=6000",
            "--dir=/sdcard/.WE",
            "--once",
            "--quiet",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(a.port, 6000);
        assert_eq!(a.dir, "/sdcard/.WE");
        assert!(a.once && a.quiet);
    }

    #[test]
    fn rejects_bad_token_and_unknown_flag() {
        assert!(args(&["--token", "zz"]).is_err());
        assert!(args(&["--nope"]).is_err());
        assert!(args(&[]).is_err());
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(args(&["-h"]).unwrap().is_none());
    }
}
