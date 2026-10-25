//! BasketLink：分享出去的那条链接。见 ADR-0002。

use std::fmt;
use std::net::{IpAddr, UdpSocket};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::basket::BasketId;

/// 局域网那条路（NFS 导出）用的固定端口。
///
/// 固定是为了不把它塞进链接里：链接的格式就不用动了。代价是端口被占就起不来，
/// 但那是启动时的响亮失败，不是静默失败。
pub const NFS_PORT: u16 = 20490;

/// 网页那条路（第二个入口）用的固定端口。
///
/// 跟 NFS 分开：一个是内核协议，一个是给浏览器看的。固定、不进链接，理由同上。
/// 对方点链接看到的地址形如 `http://<主机>:20480/b/<篮子id>/`。
pub const HTTP_PORT: u16 = 20480;

/// 网页端口。`CHERRYSHARE_HTTP_PORT` 只是开发、测试时把第二个实例摆到别处用的，
/// 不是给用户用的（跟 `CHERRYSHARE_DATA_DIR` 一类）。
pub fn http_port() -> u16 {
    std::env::var("CHERRYSHARE_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(HTTP_PORT)
}

/// 分享的单位就是一个 link。
///
/// 文本形态是 `cherryshare:<地址>:<id>`。自己发明这个格式（不再借
/// `iroh-tickets`）是因为它只有十几行，而**可读性在这里是功能**：这条链接会被人
/// 粘进聊天窗口，也可能被人看一眼。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasketLink {
    /// 对方怎么找到创建者。见 [`Host`]。
    pub host: Host,
    /// 凭证，不是索引：谁拿到谁就能看。吊销 = 换一个新的 `basket_id`
    /// （删掉文件里 `id` 那行再让它重发也行）。
    pub basket_id: BasketId,
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
        // 这串东西会被原样拼进挂载命令里，人要拷进 shell 粘。
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

/// 这台机器能拿出去的地址，按该递给对方的先后来排：本机名在前，局域网 IP 在后。
///
/// 两条一起给，因为发送方判不出对方在不在同一个广播域（ADR-0002）：名字过不了路由和
/// 隧道，IP 这边一换 DHCP 地址就废了。拿不到的就不放进来——容器里常常没有局域网地址。
pub fn candidate_hosts() -> Vec<Host> {
    let mut out = Vec::new();
    if let Some(name) = local_hostname() {
        out.push(name);
    }
    if let Ok(ip) = lan_addr() {
        out.push(Host::Ip(ip));
    }
    out
}

/// 在候选里挑一条跟 `current` 种类不同的：名字配 IP，IP 配名字。
///
/// 抽出来是为了能单测——真实的候选列要问网卡，测里造不出来（跟 [`usable_by_flags`] 一个道理）。
fn other_kind<'a>(current: &Host, candidates: &'a [Host]) -> Option<&'a Host> {
    candidates.iter().find(|h| {
        matches!(
            (h, current),
            (Host::Name(_), Host::Ip(_)) | (Host::Ip(_), Host::Name(_))
        )
    })
}

/// 网页上「挂载说明」默认跟来访者访问时用的地址走，点切换按钮时换到这一个。
///
/// 「哪两条地址是一对」只由 [`candidate_hosts`] 说了算，不在这儿另写一份。
pub fn alternate_host(current: &Host) -> Option<Host> {
    other_kind(current, &candidate_hosts()).cloned()
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
    pub fn new(host: Host, basket_id: BasketId) -> Self {
        Self { host, basket_id }
    }

    /// 挂载路径、网页 URL 里那一段。它是唯一的权威，`cherryd` 那边解析它。
    pub fn basket_id(&self) -> &str {
        self.basket_id.as_str()
    }

    /// [`Self::basket_id`] 的反函数：从 URL 或挂载路径里那段字符串认回一个 id。
    pub fn parse_basket_id(s: &str) -> Option<BasketId> {
        BasketId::parse(s).ok()
    }

    /// 挂载点的最后一段：`<名字>-<id 前 8 位>`。两个系统各自拼自己的父目录
    /// （macOS 是 `~/cherry/`，Linux 是 `/mnt/`）。
    ///
    /// 这段要进对方要粘的那行 shell 命令，所以名字得先**投影**成一个安全词：字母数字和
    /// `-` `_` `.` 之外的一律换成 `-`。这样命令里一个引号都不用写，也不会漏掉哪个字符。
    /// 篮子名本身不改——它就是文件名，你手打的、`ls` 里看到的，得是同一个东西。
    ///
    /// 那 8 位是防重名的：不同人发过来的篮子可能同名，而挂载命令是在**对方**机器上执行的，
    /// 我们不知道他那儿挂了什么。投影之后撞名（`a b` 和 `a-b`）也靠它分开。
    ///
    /// 注意 **Finder 里显示的不是这个名字**：那用的是卷名，NFS 取的是导出路径的最后一段
    /// （也就是完整 id），改不了。
    pub fn mountpoint_leaf(&self, basket_name: &str) -> String {
        let short = self.basket_id.short();
        if basket_name.is_empty() {
            short.to_owned()
        } else {
            format!("{}-{short}", safe_leaf(basket_name))
        }
    }

    /// 挂载命令里的 `server:/path`。
    pub fn mount_source(&self) -> String {
        format!("{}:/{}", self.host, self.basket_id)
    }

    /// macOS 的挂载点。两个系统各自拼自己的父目录，但“叫什么”只此一处。
    pub fn macos_mountpoint(&self, basket_name: &str) -> String {
        format!("~/cherry/{}", self.mountpoint_leaf(basket_name))
    }

    /// Linux 的挂载点。
    pub fn linux_mountpoint(&self, basket_name: &str) -> String {
        format!("/mnt/{}", self.mountpoint_leaf(basket_name))
    }

    /// 网页地址：对方点这个就能看、能下，不用挂。
    ///
    /// 端口得写出来（HTTP_PORT 不是 80）。凭证就是路径里那一段 id。
    pub fn web_url(&self) -> String {
        format!("http://{}:{}/b/{}/", self.host, http_port(), self.basket_id)
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
        // `ro`：这个导出只读，客户端挂成只读，往卷里拖东西会出禁止标记、`mount` 里也看得到。
        // 权限位是照实报的（见 `cherryd::nfs`），所以「只读」这件事由挂载选项和写失败来说，
        // 不由权限位冒充——冒充的代价是从共享里拷出来的东西全是只读的。
        format!(
            "{mk} {mountpoint} && {prog} -o ro,{locks},vers=3,tcp,port={port},mountport={port},soft,actimeo=5 {} {mountpoint}",
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

/// 挑一个局域网地址——链接里要填的那个。
///
/// **不能只问内核“要去外网走哪张网卡”**——有 VPN 的时候默认路由走隧道，拿到的就是
/// 隧道地址，而那个地址对方的局域网里到不了（实测：`utun10` 的 172.19.0.1 挂不上，
/// 报 `Connection reset by peer`，而真网卡 `en0` 的 10.1.2.52 正常）。
///
/// 所以先按**接口标志**筛出“真正的网卡”：开着、能广播、不是回环、不是点对点
/// （VPN 隧道就是点对点的）。然后在这个集合里，如果内核的默认出口地址也在里面，
/// 就用它——那是内核的意见，而且已经被筛子验证过；不在就用第一个私网地址。
///
/// 已知不完美：虚拟机的网桥接口也可能有私网 IPv4 且能广播，会混进候选。挑错了也不致命：
/// `cherry list` 把名字和 IP 两条一起给人，对方挑能用的那条（[candidate_hosts]）。
pub fn lan_addr() -> Result<IpAddr> {
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
    Ok(match route {
        // 内核的默认出口地址，得先通过筛子才算数。
        Some(a) if candidates.contains(&a) => a,
        _ => candidates[private_first],
    })
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

/// 挂载点里那段名字投影成一个「合法词」：字母数字和 `-` `_` `.` 留着，其余一律换成
/// 一个 `-`（连续的合成一个），两头多余的 `-` 剪掉。
///
/// 这段会被拼进对方要粘进 shell 的那行命令，所以这里按最严的字符集收一遍，命令里就一个
/// 引号都不用写。投影完什么都不剩（名字全是符号）就退成 `basket`。
fn safe_leaf(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "basket".to_owned()
    } else {
        trimmed.to_owned()
    }
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
        write!(f, "cherryshare:{}:{}", self.host, self.basket_id)
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
        let (host, id) = rest
            .rsplit_once(':')
            .context("链接里没有地址和 id 的分隔")?;
        let host = Host::parse(host).context("链接里那段地址用不了")?;
        let basket_id = BasketId::parse(id).context("链接里的 basket id 用不了")?;
        Ok(Self { host, basket_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BasketLink {
        BasketLink::new(
            Host::parse("192.168.8.195").unwrap(),
            BasketId::from_bytes([7u8; 16]),
        )
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
        let link = BasketLink::new(
            Host::parse("fd00::1").unwrap(),
            BasketId::from_bytes([1u8; 16]),
        );
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
        let e = "cherryshare:1.2.3.4:a/b".parse::<BasketLink>().unwrap_err();
        assert!(format!("{e:#}").contains("id"), "{e:#}");
    }

    #[test]
    fn 名字也能进链接() {
        let link = BasketLink::new(
            Host::parse("mbpmax.local").unwrap(),
            BasketId::from_bytes([3u8; 16]),
        );
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
    fn 挑出来的地址是干净的() {
        // 没网就是没网，这不是测试失败。
        if let Ok(a) = lan_addr() {
            let IpAddr::V4(v4) = a else {
                panic!("只该出 IPv4：{a}")
            };
            assert!(!v4.is_loopback(), "回环不该被挑出来：{v4}");
        }
    }

    #[test]
    fn 挂载点用篮子名加短_id() {
        let link = sample();
        assert_eq!(link.mountpoint_leaf("我的照片"), "我的照片-07070707");
        assert_eq!(link.mountpoint_leaf(""), "07070707", "没名字就只剩 id");
        // 两个同名篮子靠短 id 分开。
        let other = BasketLink::new(sample().host, BasketId::from_bytes([8u8; 16]));
        assert_ne!(link.mountpoint_leaf("照片"), other.mountpoint_leaf("照片"));
    }

    /// 名字里什么都能有（它就等于文件名），但挂载点那段只能是安全字符——那行命令不带引号。
    #[test]
    fn 挂载点那段是投影过的安全词() {
        let link = sample();
        for (raw, want) in [
            ("我的 照片", "我的-照片"),
            ("a'b", "a-b"),
            ("$(id)", "id"),
            ("a  b", "a-b"),
            ("--x--", "x"),
            ("!!", "basket"),
        ] {
            let leaf = link.mountpoint_leaf(raw);
            assert_eq!(leaf, format!("{want}-07070707"), "{raw}");
        }
        // 投影之后撞了名字也没事：后面那 8 位不一样。
        let other = BasketLink::new(sample().host, BasketId::from_bytes([8u8; 16]));
        assert_ne!(
            link.mountpoint_leaf("a b"),
            other.mountpoint_leaf("a-b"),
            "投影撞名得靠短 id 分开"
        );
    }

    #[test]
    fn 挂载命令里带地址和_id() {
        let link = sample();
        for os in [MountOs::Macos, MountOs::Linux] {
            let cmd = link.mount_command(os, "~/cherry/x");
            assert!(cmd.contains("192.168.8.195:/"), "{cmd}");
            assert!(cmd.contains(link.basket_id()), "{cmd}");
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

    /// 手写的 id 原样进链接、原样认回来；收发两边不查长度，也不规范大小写。
    #[test]
    fn 手写的_id_原样进出链接() {
        let link = BasketLink::new(sample().host, BasketId::parse("photos").unwrap());
        assert_eq!(link.mount_source(), "192.168.8.195:/photos");
        let s = link.to_string();
        assert_eq!(s, "cherryshare:192.168.8.195:photos");
        assert_eq!(s.parse::<BasketLink>().unwrap(), link);
        assert_eq!(
            BasketLink::parse_basket_id("Photos").unwrap().as_str(),
            "Photos"
        );
        for bad in ["", "a/b", "a:b", "a b"] {
            assert_eq!(BasketLink::parse_basket_id(bad), None, "该拒：{bad:?}");
        }
    }

    #[test]
    fn 另一条地址跟当前种类相反() {
        let name = Host::parse("mbp.local").unwrap();
        let ip = Host::parse("10.1.2.52").unwrap();
        let both = [name.clone(), ip.clone()];
        assert_eq!(other_kind(&name, &both), Some(&ip));
        assert_eq!(other_kind(&ip, &both), Some(&name));
        assert_eq!(other_kind(&name, &both[..1]), None, "只有一条就没得换");
    }
}
