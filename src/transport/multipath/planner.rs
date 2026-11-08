//! 链路规划：决定这一次传输用哪几条通道。
//!
//! 纯函数、无 IO，输入是探测结果，输出是链路清单。策略刻意保守：
//! **只有两条及以上链路才值得启用**——单链路跑分块协议不会比 `adb push` 快，
//! 却多了一层故障面。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// 经 `adb forward` 映射到本机回环的 USB 链路
    Adb,
    /// 局域网直连手机（`ip:port`）
    Lan(String),
}

impl LinkKind {
    pub fn label(&self) -> String {
        match self {
            LinkKind::Adb => "USB(adb)".into(),
            LinkKind::Lan(addr) => format!("WiFi({addr})"),
        }
    }

    /// 连接目标：adb 链路由 forward 映射到 127.0.0.1。
    pub fn endpoint(&self, port: u16) -> String {
        match self {
            LinkKind::Adb => format!("127.0.0.1:{port}"),
            LinkKind::Lan(addr) => addr.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Probe {
    /// agent 已投放并成功应答
    pub agent_ready: bool,
    /// adb 通道可用（能 forward）
    pub adb_ok: bool,
    /// 手机在局域网中的可达地址（`ip:port`），探测不到为 None
    pub lan_addr: Option<String>,
}

/// 返回应当启用的链路（可能为空 = 不启用多通道）。
pub fn plan(p: &Probe) -> Vec<LinkKind> {
    if !p.agent_ready {
        return Vec::new();
    }
    let mut links = Vec::new();
    if p.adb_ok {
        links.push(LinkKind::Adb);
    }
    if let Some(addr) = &p.lan_addr {
        let addr = addr.trim();
        if !addr.is_empty() {
            links.push(LinkKind::Lan(addr.to_string()));
        }
    }
    links
}

/// 是否值得走多通道（两条链路才有叠加意义）。
pub fn worth_enabling(links: &[LinkKind]) -> bool {
    links.len() >= 2
}

/// 是否真的启用。
///
/// `allow_single` 只给调试/无 WiFi 场景用：单链路跑分块协议不会更快，
/// 但它让协议栈能在真机上被验证，默认关闭。
pub fn usable(links: &[LinkKind], allow_single: bool) -> bool {
    !links.is_empty() && (worth_enabling(links) || allow_single)
}

/// 从 `adb shell ip -4 addr` 的输出里挑出可用的局域网 IPv4。
///
/// 优先 `wlan*`（真局域网），其次才是其它私有地址（热点/以太网）。
/// 移动数据网段也可能命中私有段，所以调用方还会做一次 socket 连通性探测兜底。
pub fn parse_lan_ip(output: &str) -> Option<String> {
    let mut wlan: Option<String> = None;
    let mut other: Option<String> = None;
    for line in output.lines() {
        let line = line.trim();
        // 只认 `inet <ip>/<prefix>` 这一段的地址；同一行的 `brd 192.168.x.255`
        // 也是私有地址，按 token 盲扫会把广播地址当成手机 IP。
        let Some(pos) = line.find("inet ") else {
            continue;
        };
        let rest = &line[pos + 5..];
        let Some(tok) = rest.split_whitespace().next() else {
            continue;
        };
        let ip = tok.split('/').next().unwrap_or("");
        if !is_private_v4(ip) {
            continue;
        }
        if line.contains("wlan") {
            wlan = Some(ip.to_string());
        } else {
            other = Some(ip.to_string());
        }
    }
    wlan.or(other)
}

fn is_private_v4(s: &str) -> bool {
    let mut it = s.split('.');
    let (Some(a), Some(b), Some(c), Some(_d), None) =
        (it.next(), it.next(), it.next(), it.next(), it.next())
    else {
        return false;
    };
    let (Ok(a), Ok(b)) = (a.parse::<u8>(), b.parse::<u8>()) else {
        return false;
    };
    if c.parse::<u8>().is_err() {
        return false;
    }
    if a == 10 {
        return true;
    }
    if a == 192 && b == 168 {
        return true;
    }
    if a == 172 && (16..=31).contains(&b) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(agent: bool, adb: bool, lan: Option<&str>) -> Probe {
        Probe {
            agent_ready: agent,
            adb_ok: adb,
            lan_addr: lan.map(str::to_string),
        }
    }

    #[test]
    fn no_agent_means_no_links() {
        assert!(plan(&probe(false, true, Some("192.168.1.9:5740"))).is_empty());
    }

    #[test]
    fn single_link_is_not_worth_it() {
        let links = plan(&probe(true, true, None));
        assert_eq!(links.len(), 1);
        assert!(!worth_enabling(&links), "单链路应回落 adb push，而不是跑分块协议");
    }

    #[test]
    fn adb_plus_lan_is_worth_it() {
        let links = plan(&probe(true, true, Some("192.168.1.9:5740")));
        assert_eq!(
            links,
            vec![LinkKind::Adb, LinkKind::Lan("192.168.1.9:5740".into())]
        );
        assert!(worth_enabling(&links));
        assert_eq!(links[0].endpoint(5740), "127.0.0.1:5740");
        assert_eq!(links[1].endpoint(5740), "192.168.1.9:5740");
    }

    #[test]
    fn usable_requires_two_links_unless_debug_flag() {
        let one = plan(&probe(true, true, None));
        assert!(!usable(&one, false));
        assert!(usable(&one, true));
        let none: Vec<LinkKind> = Vec::new();
        assert!(!usable(&none, true), "零链路时调试开关也不能启用");
    }

    #[test]
    fn lan_only_is_not_worth_it() {
        let links = plan(&probe(true, false, Some("192.168.1.9:5740")));
        assert!(!worth_enabling(&links));
    }

    #[test]
    fn parses_wlan_ip_from_ip_addr_output() {
        let out = "\
2: wlan0: <BROADCAST,MULTICAST,UP> mtu 1500
    inet 192.168.31.42/24 brd 192.168.31.255 scope global wlan0
";
        assert_eq!(parse_lan_ip(out).as_deref(), Some("192.168.31.42"));
    }

    #[test]
    fn prefers_wlan_over_cellular() {
        let out = "    inet 10.21.33.7/24 brd 10.21.33.255 scope global rmnet0\n    inet 192.168.31.42/24 brd 192.168.31.255 scope global wlan0\n";
        assert_eq!(parse_lan_ip(out).as_deref(), Some("192.168.31.42"));
    }

    #[test]
    fn ignores_loopback() {
        assert!(parse_lan_ip("    inet 127.0.0.1/8 scope host lo").is_none());
        assert!(parse_lan_ip("").is_none());
    }
}
