//! BasketLink：分享出去的那条链接。见 ADR-0002。

use std::fmt;
use std::net::{IpAddr, UdpSocket};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

/// 局域网那条路（NFS 导出）用的固定端口。
///
/// 固定是为了不把它塞进链接里：链接的格式就不用动了。代价是端口被占就起不来，
/// 但那是启动时的响亮失败，不是静默失败。
pub const NFS_PORT: u16 = 20490;

/// 分享的单位就是一个 link。
///
/// 文本形态是 `cherryshare:<地址>:<32 位十六进制>`。自己发明这个格式（不再借
/// `iroh-tickets`）是因为它只有十几行，而**可读性在这里是功能**：这条链接会被人
/// 粘进聊天窗口，也可能被人看一眼。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasketLink {
    /// 对方怎么找到创建者。见 [`Host`]。
    pub host: Host,
    /// 凭证，不是索引：谁拿到谁就能看。吊销 = 换一个新的 `basket_id`。
    pub basket_id: [u8; 16],
}

/// 链接和挂载命令里那一段地址：一个 IP，或者对方能解析的一个名字。
///
/// 名字这一路是给**同一个广播域**的对方用的。`.local` 是 mDNS，组播，过不了路由器，
/// 也过不了隧道（实测：你机器上那串全隧道把 10.1.55.11、10.1.0.22 都兜走了，只有
/// 同网段的对方能用名字）。换来的是换 DHCP 地址之后链接还成立。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Ip(IpAddr),
    /// 已经校验过字符集的名字，见 [`Host::parse`]。
    Name(String),
}

impl Host {
    /// 能当 IP 就当 IP，否则按名字校验。
    ///
    /// IP 先判：IPv6 里带冒号，字符集那一关它过不去。
    pub fn parse(s: &str) -> Result<Self> {
        // 链接里抠出来的 IPv6 是带方括号的，命令行上手打也可能带上。
        let bare = s
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .unwrap_or(s);
        if let Ok(ip) = bare.parse::<IpAddr>() {
            return Ok(Self::Ip(ip));
        }

        if s.is_empty() || s.len() > 253 {
            bail!("`{s}` 当主机名太短或太长");
        }
        // 这串东西会被原样拼进 `cherry link` 打出来的那行挂载命令里，人要拷进 shell 粘。
        // 所以字符集卡死：不给引号、分号、空白、斜杠留位置。冒号也不行——链接是按
        // 冒号切的。
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            bail!("`{s}` 里有当不了主机名的字符（只给字母数字和 `-` `_` `.` 留位置）");
        }
        if s.starts_with('-') {
            bail!("`{s}` 以 `-` 开头，会被挂载命令当成选项");
        }
        Ok(Self::Name(s.to_owned()))
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // IPv6 要套方括号，不然那个冒号会被当成分隔符。
            Self::Ip(IpAddr::V6(a)) => write!(f, "[{a}]"),
            Self::Ip(IpAddr::V4(a)) => write!(f, "{a}"),
            Self::Name(n) => write!(f, "{n}"),
        }
    }
}

/// 本机的 mDNS 名（`mbpmax.local` 这种），当候选印出来用。
///
/// 拿的是 `gethostname(2)`。macOS 上它给的就是 Bonjour 名；Linux 上得看 avahi 怎么配。
/// **所以它只当候选，不当默认**——猜错了对方就挂不上，而对方挂不上的时候我们这边
/// 什么也看不见。
pub fn local_hostname() -> Option<Host> {
    let mut buf = [0u8; 256];
    // SAFETY: 传的是我们自己这块缓冲区的指针和长度，`gethostname` 只会往里写。
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    let name = std::str::from_utf8(&buf[..end]).ok()?;
    if name.is_empty() {
        return None;
    }
    // mDNS 的名字必须带 `.local`。有些系统给的就是全名，有些只给主机名。
    let name = if name.contains('.') {
        name.to_owned()
    } else {
        format!("{name}.local")
    };
    Host::parse(&name).ok()
}

/// 对方那边是什么系统。
///
/// **两个系统都得给，因为对方的机器我们不知道**，而这条链接就是唯一的口子：
/// `cherry mount` 已经删了（ADR-0007 决策七），没有第二条路。两边差别不只是多不多一个 s：
/// 选项名一个 `nolocks` 一个 `nolock`，Linux 挂载还要 root。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountOs {
    Macos,
    Linux,
}

impl BasketLink {
    pub fn new(host: Host, basket_id: [u8; 16]) -> Self {
        Self { host, basket_id }
    }

    /// 挂载路径里那一段。它是唯一的权威，`cherryd` 那边解析它。
    pub fn basket_id_hex(&self) -> String {
        hex::encode(self.basket_id)
    }

    /// [`Self::basket_id_hex`] 的反函数。
    pub fn parse_basket_id_hex(s: &str) -> Option<[u8; 16]> {
        hex::decode(s).ok()?.try_into().ok()
    }

    /// 挂载点的最后一段：`<篮子名>-<id 前 8 位>`。两个系统各自拼自己的父目录
    /// （macOS 是 `~/cherry/`，Linux 是 `/mnt/`）。
    ///
    /// 名字是给人读的（`ls ~/cherry` 一眼能认出来），那 8 位是防重名的：不同人发过来的
    /// 篮子可能同名，而挂载命令是在**对方**机器上执行的，我们不知道他那儿挂了什么。
    /// 篮子名允许哪些字符由 `basket::validate_basket_name` 管（它得能安全地进这行命令）。
    ///
    /// 注意 **Finder 里显示的不是这个名字**：那用的是卷名，NFS 取的是导出路径的最后一段
    /// （也就是完整 id），改不了。
    pub fn mountpoint_leaf(&self, basket_name: &str) -> String {
        let short = &self.basket_id_hex()[..8];
        if basket_name.is_empty() {
            short.to_owned()
        } else {
            format!("{basket_name}-{short}")
        }
    }

    /// 挂载命令里的 `server:/path`。
    pub fn mount_source(&self) -> String {
        format!("{}:/{}", self.host, self.basket_id_hex())
    }

    /// 对方要粘的那一行命令（`mkdir` 和挂载串在一起，粘一次就行）。
    ///
    /// `actimeo=5` 是属性缓存秒数：它决定创建者增删一颗 cherry 之后对方多久看得见。
    /// 给小一点贴合"篮子里的东西是活的"这个语义，代价是往返变多。
    ///
    /// `nolocks` / `nolock`：关掉 NLM 锁。我们不跑锁服务（也不需要——导出是只读的），
    /// 不关的话客户端会先去找一遍 portmapper，白白多一跳甚至挂不上。
    pub fn mount_command(&self, os: MountOs, mountpoint: &str) -> String {
        let port = NFS_PORT;
        let (mk, prog, locks) = match os {
            MountOs::Macos => ("mkdir -p", "mount_nfs", "nolocks"),
            // Linux 上挂载要 root，而 `-t nfs` 是 mount(8) 的用法。
            MountOs::Linux => ("sudo mkdir -p", "sudo mount -t nfs", "nolock"),
        };
        format!(
            "{mk} {mountpoint} && {prog} -o {locks},vers=3,tcp,port={port},mountport={port},soft,actimeo=5 {} {mountpoint}",
            self.mount_source()
        )
    }

    /// 卸掉。macOS 上普通人就能卸，Linux 上要 root。
    pub fn unmount_command(os: MountOs, mountpoint: &str) -> String {
        match os {
            MountOs::Macos => format!("umount {mountpoint}"),
            MountOs::Linux => format!("sudo umount {mountpoint}"),
        }
    }
}

/// 链接里该填哪个地址，以及还有哪些备选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanAddrs {
    /// 挑出来的那个。
    pub preferred: IpAddr,
    /// 其余候选：让人在对方挂不上的时候换着试（`cherry link --host`）。
    pub others: Vec<IpAddr>,
}

/// 挑一个局域网地址。
///
/// **不能只问内核“要去外网走哪张网卡”**——有 VPN 的时候默认路由走隧道，拿到的就是
/// 隧道地址，而那个地址对方的局域网里到不了（实测：`utun10` 的 172.19.0.1 挂不上，
/// 报 `Connection reset by peer`，而真网卡 `en0` 的 10.1.2.52 正常）。
///
/// 所以先按**接口标志**筛出“真正的网卡”：开着、能广播、不是回环、不是点对点
/// （VPN 隧道就是点对点的）。然后在这个集合里，如果内核的默认出口地址也在里面，
/// 就用它——那是内核的意见，而且已经被筛子验证过；不在就用第一个私网地址。
///
/// 已知不完美：虚拟机的网桥接口也可能有私网 IPv4 且能广播，会混进候选。所以候选
/// 全部返回，由调用方（`cherry link`）印出来让人换着试。
pub fn lan_addrs() -> Result<LanAddrs> {
    let mut candidates = broadcast_ipv4s();
    if candidates.is_empty() {
        bail!("这台机器上没有可用的局域网地址（没连网？）");
    }
    candidates.sort_unstable();

    // “私网优先”是为了把链路本地之类排到后面。
    let private_first = candidates
        .iter()
        .position(|a| matches!(a, IpAddr::V4(v4) if v4.is_private()))
        .unwrap_or(0);

    let route = route_source_addr();
    let preferred = match route {
        // 内核的默认出口地址，得先通过筛子才算数。
        Some(a) if candidates.contains(&a) => a,
        _ => candidates[private_first],
    };
    let others = candidates.into_iter().filter(|a| *a != preferred).collect();
    Ok(LanAddrs { preferred, others })
}

/// 按接口标志筛出真正的网卡上的 IPv4 地址。
///
/// 用 `getifaddrs` 是因为标志（广播 / 点对点）在纯 std 里拿不到，而它们正是
/// 区分“网卡”和“隧道”的东西。
fn broadcast_ipv4s() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // SAFETY: 只调用 `getifaddrs` 系列，按 C 的约定成对释放；每个指针在使用前
    // 都查过非空，地址族也按 `sockaddr_in` 解释前先确认是 `AF_INET`。
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as i32;
            if !usable_by_flags(flags) {
                continue;
            }
            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
            // `s_addr` 是网络字节序，`Ipv4Addr::from(u32)` 要的是大端数值。
            let v4 = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
            out.push(IpAddr::V4(v4));
        }
        libc::freeifaddrs(head);
    }
    out
}

/// 只看接口标志：这算不算一张“真网卡”。
///
/// 抽出来是为了能单测——真实的接口标志没法在测试里造出来，而这条规则是这个函数
/// 存在的全部理由（VPN 隧道就是 `IFF_POINTOPOINT`）。
fn usable_by_flags(flags: i32) -> bool {
    let want = libc::IFF_UP | libc::IFF_BROADCAST;
    let reject = libc::IFF_LOOPBACK | libc::IFF_POINTOPOINT;
    flags & want == want && flags & reject == 0
}

/// 内核认为的“出口”地址：问它“去外网走哪张网卡”。
///
/// 拿一个 UDP socket `connect` 到一个公网地址——**一个包都不发**，只是让内核选一次
/// 路由，然后问它本地那头是哪个地址。选错也无所谓，它只是候选之一。
fn route_source_addr() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // 8.8.8.8 只是“随便一个要出门的地址”，不解析、不发包。
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip())
}

impl fmt::Display for BasketLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cherryshare:{}:{}", self.host, self.basket_id_hex())
    }
}

impl FromStr for BasketLink {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let rest = s
            .trim()
            .strip_prefix("cherryshare:")
            .context("链接要以 `cherryshare:` 开头")?;
        // 从右边切最后一个 ':'，这样带冒号的 IPv6 也不会切错。
        let (host, id_hex) = rest
            .rsplit_once(':')
            .context("链接里没有地址和 id 的分隔")?;
        let host = Host::parse(host).context("链接里那段地址用不了")?;
        let basket_id =
            Self::parse_basket_id_hex(id_hex).context("链接里的 basket id 不是 32 位十六进制")?;
        Ok(Self { host, basket_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BasketLink {
        BasketLink::new(Host::parse("192.168.8.195").unwrap(), [7u8; 16])
    }

    #[test]
    fn 往返一致() {
        let link = sample();
        let s = link.to_string();
        assert_eq!(s, format!("cherryshare:192.168.8.195:{}", "07".repeat(16)));
        assert_eq!(s.parse::<BasketLink>().unwrap(), link);
    }

    #[test]
    fn ipv6_要套方括号且能解析回来() {
        let link = BasketLink::new(Host::parse("fd00::1").unwrap(), [1u8; 16]);
        let s = link.to_string();
        assert!(s.contains("[fd00::1]"), "{s}");
        assert_eq!(s.parse::<BasketLink>().unwrap(), link);
    }

    #[test]
    fn 坏链接要说清楚哪里坏() {
        assert!("nope".parse::<BasketLink>().is_err());
        assert!(
            "cherryshare:1.2.3.4".parse::<BasketLink>().is_err(),
            "缺 id"
        );
        // 名字是允许的，所以坏的是那串字符本身。
        let e = "cherryshare:a b:07".parse::<BasketLink>().unwrap_err();
        assert!(format!("{e:#}").contains("主机名"), "{e:#}");
        let e = "cherryshare:1.2.3.4:zz".parse::<BasketLink>().unwrap_err();
        assert!(e.to_string().contains("32 位十六进制"), "{e}");
    }

    #[test]
    fn 名字也能进链接() {
        let link = BasketLink::new(Host::parse("mbpmax.local").unwrap(), [3u8; 16]);
        let s = link.to_string();
        assert_eq!(s, format!("cherryshare:mbpmax.local:{}", "03".repeat(16)));
        assert_eq!(s.parse::<BasketLink>().unwrap(), link);
        for os in [MountOs::Macos, MountOs::Linux] {
            let cmd = link.mount_command(os, "~/cherry/x");
            assert!(cmd.contains("mbpmax.local:/"), "{cmd}");
        }
    }

    /// 这串字符会被原样拼进那行要粘进 shell 的命令里，所以只能放这几种。
    #[test]
    fn 名字里的坏字符要拒() {
        for bad in [
            "a;rm -rf ~",
            "a b",
            "a/b",
            "-x",
            "$(id)",
            "a\nb",
            "a:20490",
            "",
        ] {
            assert!(Host::parse(bad).is_err(), "该拒：{bad:?}");
        }
        for ok in [
            "mbpmax.local",
            "nas",
            "my-box_2.local",
            "10.1.2.52",
            "fd00::1",
        ] {
            assert!(Host::parse(ok).is_ok(), "该收：{ok}");
        }
    }

    #[test]
    fn 只有真网卡才算数() {
        // 开着的、能广播的（以太网 / Wi-Fi）
        assert!(usable_by_flags(libc::IFF_UP | libc::IFF_BROADCAST));
        // 回环不行
        assert!(!usable_by_flags(
            libc::IFF_UP | libc::IFF_BROADCAST | libc::IFF_LOOPBACK
        ));
        // 点对点的不行——VPN 隧道（utun）就是这个。
        // 实测：拿它的地址去挂，报 `Connection reset by peer`。
        assert!(!usable_by_flags(libc::IFF_UP | libc::IFF_POINTOPOINT));
        // 没起来的不行
        assert!(!usable_by_flags(libc::IFF_BROADCAST));
        assert!(!usable_by_flags(0));
    }

    #[test]
    fn 筛出来的地址里不能有回环与隧道() {
        // 不假设这台机器连了网（CI 上可能没有），只查“筛出来的必须干净”。
        for a in broadcast_ipv4s() {
            let IpAddr::V4(v4) = a else {
                panic!("只该出 IPv4：{a}")
            };
            assert!(!v4.is_loopback(), "回环不该出现：{v4}");
        }
    }

    #[test]
    fn 首选不在备选里() {
        // 没网就是没网，这不是测试失败。
        if let Ok(a) = lan_addrs() {
            assert!(!a.others.contains(&a.preferred), "{a:?}");
        }
    }

    #[test]
    fn 挂载点用篮子名加短_id() {
        let link = sample();
        assert_eq!(link.mountpoint_leaf("我的照片"), "我的照片-07070707");
        assert_eq!(link.mountpoint_leaf(""), "07070707", "没名字就只剩 id");
        // 两个同名篮子靠短 id 分开。
        let other = BasketLink::new(sample().host, [8u8; 16]);
        assert_ne!(link.mountpoint_leaf("照片"), other.mountpoint_leaf("照片"));
    }

    #[test]
    fn 挂载命令里带地址和_id() {
        let link = sample();
        for os in [MountOs::Macos, MountOs::Linux] {
            let cmd = link.mount_command(os, "~/cherry/x");
            assert!(cmd.contains("192.168.8.195:/"), "{cmd}");
            assert!(cmd.contains(&link.basket_id_hex()), "{cmd}");
            assert!(cmd.contains(&format!("port={NFS_PORT}")), "{cmd}");
        }
    }

    #[test]
    fn 两个系统的挂载命令不一样() {
        let link = sample();
        let mac = link.mount_command(MountOs::Macos, "/mnt/x");
        let lin = link.mount_command(MountOs::Linux, "/mnt/x");
        assert!(mac.contains("nolocks"), "{mac}");
        assert!(lin.contains("nolock,"), "{lin}");
        assert!(!lin.contains("nolocks"), "Linux 的选项名不带 s：{lin}");
        assert!(mac.contains("mount_nfs"), "{mac}");
        assert!(
            lin.contains("sudo mount -t nfs"),
            "Linux 上挂载要 root：{lin}"
        );
        assert_eq!(
            BasketLink::unmount_command(MountOs::Macos, "/mnt/x"),
            "umount /mnt/x"
        );
        assert_eq!(
            BasketLink::unmount_command(MountOs::Linux, "/mnt/x"),
            "sudo umount /mnt/x"
        );
    }

    #[test]
    fn 篮子_id_的十六进制能来回() {
        let id = [
            0x7f, 0xcb, 0xce, 0xab, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];
        let link = BasketLink::new(sample().host, id);
        assert_eq!(link.basket_id_hex().len(), 32);
        assert_eq!(
            BasketLink::parse_basket_id_hex(&link.basket_id_hex()),
            Some(id)
        );
        assert_eq!(BasketLink::parse_basket_id_hex("7fcbceab"), None, "短了");
        assert_eq!(
            BasketLink::parse_basket_id_hex(&"z".repeat(32)),
            None,
            "不是十六进制"
        );
        assert_eq!(BasketLink::parse_basket_id_hex(""), None);
    }
}
