//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004）。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。
//!
//! 这里**只有读命令**：要建篮子、加东西、删东西，就去数据目录里改那个 `.basket` 文件
//! （ADR-0008）。所以这个程序不需要知道你的 cwd，也不需要把路径变成绝对路径。

use anyhow::{Result, bail};
use cherry_control::{BasketView, BrokenView, Client, Request, Response};
use cherry_core::BasketLink;
use cherry_core::link::Host;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "cherry",
    // 敲 `cherry` 本身就是问“我能干什么”，所以不带参数就把名字、版本和帮助一起打出来，
    // `-h` / `-V` 这两个 flag 不要（子命令的用法还有 clap 自带的 `cherry help <命令>`）。
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// 服务状态
    Status,
    /// 列出所有篮子：每个的文件原文、哪几行现在读不到，以及要发给对方的网页链接
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    // `cherry … | head` 这种用法会让读的那头提前关掉管道。Rust 默认把 SIGPIPE 设成忽略，
    // 于是 `println!` 写不进去时就 panic 出一段回溯。命令行程序该做的是按系统默认处理：
    // 安静地死掉（退出码 141），跟 `ls | head` 一个样子。
    // SAFETY: 只是把信号处理恢复成默认，没有别的副作用。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let cli = Cli::parse();
    let Some(cmd) = cli.cmd else {
        // 敲 `cherry` 就是在问“这是什么、哪一版、能干什么”。这句话只在 banner 里说一遍，
        // 所以命令上没有 `about`——帮助里不再重复。
        println!(
            "CherryShare {} 好用的局域网文件共享工具",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        Cli::command().print_long_help()?;
        println!();
        return Ok(());
    };

    // 数据目录不做成选项：正常用的人就只有默认那一个，摆到帮助里是多余的。要开第二个
    // 实例（e2e）就设 `CHERRYSHARE_DATA_DIR`——`default_data_dir()` 会读。
    let data_dir = cherry_core::service::default_data_dir()?;
    let mut client = Client::connect(&data_dir).await?;
    match client.call(&to_request(cmd)).await? {
        Response::Error(e) => bail!("{e}"),
        other => print(other),
    }
    Ok(())
}

fn to_request(cmd: Cmd) -> Request {
    match cmd {
        Cmd::Status => Request::Status,
        Cmd::List => Request::List,
    }
}

/// 一个命令就是全部：文件原文 + 读不到的路径 + 链接。不再有「只看某一个」的第二个
/// 命令——列表里每一段都带文件路径，找一颗篮子就是扫一眼的事。
fn print(resp: Response) {
    match resp {
        Response::Error(_) => {}
        Response::Status {
            baskets,
            broken,
            data_dir,
            nfs_port,
            nfs_up,
            http_port,
            http_up,
            config_file,
            owner,
            config_error,
        } => {
            println!("data       {}", tilde(&data_dir));
            println!("config     {}", tilde(&config_file));
            println!("name       {owner}");
            // 读坏了要说清楚：这时候页面上那个名字是上一份，不是你刚写的那份。
            if let Some(e) = config_error {
                println!("           读不进来，页面上还是上一份：");
                for line in e.lines() {
                    println!("             {line}");
                }
            }
            println!(
                "baskets    {baskets}{}",
                if broken > 0 {
                    format!("（另有 {broken} 个文件读不进来，`cherry list` 里有原因）")
                } else {
                    String::new()
                }
            );
            // 监听没起来就说出来，别让人以为分享是好的。
            println!(
                "nfs        {nfs_port}  {}",
                if nfs_up {
                    ""
                } else {
                    "（没起来：局域网挂载这条路不可用）"
                }
            );
            println!(
                "web        {http_port}  {}",
                if http_up {
                    ""
                } else {
                    "（没起来：链接点不开）"
                }
            );
        }
        Response::Baskets { dir, ok, broken } => {
            let empty = ok.is_empty() && broken.is_empty();
            print_empty_hint(&dir, empty);
            // 目录只打一次：段头只给文件名，不然同样的前缀在每段重复一遍。
            if !empty {
                println!("data      {}", tilde(&dir));
                println!();
            }
            for (i, b) in ok.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print_basket(b);
            }
            print_broken(&broken);
        }
    }
}

/// 家目录写成 `~`：数据目录和配置文件路径几乎都以它开头，绝对路径白白吃掉一截版面。
/// 只用在给人看的输出里，程序里该用完整路径的地方照旧。
fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(|p| p.to_owned())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// 空的时候别只说「空的」：得告诉人去哪儿写文件——现在唯一的写入口就是这个。
fn print_empty_hint(dir: &std::path::Path, empty: bool) {
    if empty {
        println!(
            "（还没有 basket。在 {} 里写个 <名字>.basket，一行一条要分享的路径，以 ~/ 开头或写绝对路径；\n  想在篮子里保留目录，就写成 `前缀/./可见/路径`）",
            tilde(dir)
        );
    }
}

/// 路径的最后一段。`/a/b/x.basket` → `x.basket`。
fn file_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// 读不进来的文件单独一段，原因照原文给：那个人得知道去改哪一行。
fn print_broken(broken: &[BrokenView]) {
    if broken.is_empty() {
        return;
    }
    println!("\n这些文件没读进来（它们不会被分享）：");
    for b in broken {
        println!("  {}", b.file);
        for line in b.error.lines() {
            println!("    {line}");
        }
    }
}

/// 那条链接里的地址是名字还是 IP——两条摆在一起的时候得让人分得出哪条是哪条。
fn 地址种类(host: &Host) -> &'static str {
    match host {
        Host::Name(_) => "本机名",
        Host::Ip(_) => "本机IP",
    }
}

/// 一段一个篮子：`==> 文件名 <==` 开头；段头只给文件名（数据目录在上面打过一行），
/// 然后是文件原文（文件是唯一真相，ADR-0008，它长什么样就打成什么样，注释和 `id` 行
/// 本来就在里面），接着是读不到的路径，最后是链接。
fn print_basket(b: &BasketView) {
    println!("==> {} <==", file_name(&b.file));
    print!("{}", b.content);
    if !b.content.is_empty() && !b.content.ends_with('\n') {
        println!();
    }
    if b.content.trim().is_empty() {
        println!("（空的：在那个文件里写一行路径就有了）");
    }
    if !b.missing.is_empty() {
        println!();
        for m in &b.missing {
            println!("  读不到：{m}");
        }
    }
    println!();
    // 两条链接一起摆出来：发送方判不出对方在不在同一个广播域（ADR-0002），名字过不了
    // 路由和隧道，IP 这边一换 DHCP 地址就废了。挂载命令不在这儿打：页面上有，而且那份
    // 是按对方自己的系统给的。
    if b.links.is_empty() {
        println!("  这台机器既没有本机名也没有可用的局域网地址，链接算不出来");
        return;
    }
    for raw in &b.links {
        match raw.parse::<BasketLink>() {
            Ok(link) => println!("  {}  {}", 地址种类(&link.host), link.web_url()),
            // 解析不了就照原样打出来，别把服务给的东西吞掉。
            Err(_) => println!("  {raw}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 敲 `cherry` 不带参数得解析得动——这条路要走到打帮助，而不是让 clap 报错。
    #[test]
    fn 不带参数能解析() {
        let cli = Cli::try_parse_from(["cherry"]).unwrap();
        assert!(cli.cmd.is_none());
    }

    #[test]
    fn list_不收参数() {
        // 一个命令就是全部：不再有「只看某一个篮子」的第二个命令，也不收地址参数。
        let cli = Cli::try_parse_from(["cherry", "list"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::List)));
        assert!(Cli::try_parse_from(["cherry", "list", "--ip"]).is_err());
        assert!(Cli::try_parse_from(["cherry", "list", "--host", "10.0.0.1"]).is_err());
        assert!(Cli::try_parse_from(["cherry", "list", "t"]).is_err());
    }

    /// 写入口撤了（ADR-0008）：建篮子、加东西、删东西都是改文件，这里不该再有命令。
    #[test]
    fn 只留读命令() {
        for gone in ["new", "add", "rm", "delete", "show"] {
            assert!(
                Cli::try_parse_from(["cherry", gone, "x"]).is_err(),
                "{gone} 该没了"
            );
        }
    }

    /// 数据目录不给用户选（正常用不到），客户端就不认这个选项。
    #[test]
    fn 没有_data_dir_这个选项() {
        assert!(Cli::try_parse_from(["cherry", "--data-dir", "/tmp", "list"]).is_err());
    }
}
