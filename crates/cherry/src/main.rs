//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004）。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。
//!
//! 这里**只有读命令**：要建篮子、加东西、删东西，就去数据目录里改那个 `.basket` 文件
//! （ADR-0008）。所以这个程序不需要知道你的 cwd，也不需要把路径变成绝对路径。

use anyhow::{Result, bail};
use cherry_control::{BasketView, BrokenView, Client, LinkSet, Request, Response};
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
    /// 列出本机所有 basket，每个都给出要发给对方的网页链接——本机名一条、IP 一条
    List,
    /// 把一个 basket 文件原样打出来，顺带说哪几行现在读不到；不给篮子就全打
    Show {
        /// 篮子名（就是数据目录里那个文件名，去掉 .basket）。不给我就全看
        basket: Option<String>,
    },
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
        Cmd::List => Request::Links,
        Cmd::Show { basket } => match basket {
            Some(name) => Request::ShowBasket { name },
            None => Request::ListBaskets,
        },
    }
}

/// `cherry show` 不带给篮子时也是每颗都给详情——反正它问的就是「里面有什么」。
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
        } => {
            println!("data       {}", data_dir.display());
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
            print_empty_hint(&dir, ok.is_empty() && broken.is_empty());
            for (i, b) in ok.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print_basket(b, true);
            }
            print_broken(&broken);
        }
        Response::Basket(b) => print_basket(&b, false),
        // 一个篮子一小段：头上是名字和里面有几样东西，下面两条链接——本机名一条、IP 一条。
        // 两条一起摆出来是因为发送方判不出对方在不在同一个广播域（ADR-0002）：名字过不了
        // 路由和隧道，IP 这边一换 DHCP 地址就废了。URL 由它现场拼，这一步跟以前拼挂载命令
        // 是同一件事，纯粹是格式。
        // 挂载命令不在这儿打了：页面上有，而且那份是按对方自己的系统给的，比这边猜的准。
        Response::Links { dir, sets, broken } => {
            print_empty_hint(&dir, sets.is_empty() && broken.is_empty());
            for (i, set) in sets.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                print_links(set);
            }
            print_broken(&broken);
        }
    }
}

/// 空的时候别只说「空的」：得告诉人去哪儿写文件——现在唯一的写入口就是这个。
fn print_empty_hint(dir: &std::path::Path, empty: bool) {
    if empty {
        println!(
            "（还没有 basket。在 {} 里写个 <名字>.basket，一行一条要分享的路径，以 ~/ 开头或写绝对路径）",
            dir.display()
        );
    }
}

fn print_links(set: &LinkSet) {
    println!("{}（{} 项）  {}", set.name, set.items, set.file);
    for raw in &set.links {
        match raw.parse::<BasketLink>() {
            Ok(link) => println!("  {}  {}", 地址种类(&link.host), link.web_url()),
            // 解析不了就照原样打出来，别把服务给的东西吞掉。
            Err(_) => println!("  {raw}"),
        }
    }
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

/// 原样打文件内容。要看的就这一份东西：文件是唯一真相（ADR-0008），它长什么样就打成
/// 什么样，你写的注释也在里面，`id` 那行本来就在里面，不用再抄一遍。
///
/// 几个篮子一起打的时候才加一行 `==> 文件 <==`（跟 `head` 一个约定）：不然分不出哪段是
/// 哪个文件的。单个篮子不加：那条路径就在上面那句里，重复一遍没意思。
fn print_basket(b: &BasketView, with_header: bool) {
    if with_header {
        println!("==> {} <==", b.file);
    }
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
    fn show_不带篮子也行_list_不收地址() {
        let cli = Cli::try_parse_from(["cherry", "show"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Show { basket: None })));

        // list 自己把名字和 IP 两条都列出来，不再收地址参数。
        let cli = Cli::try_parse_from(["cherry", "list"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::List)));
        assert!(Cli::try_parse_from(["cherry", "list", "--ip"]).is_err());
        assert!(Cli::try_parse_from(["cherry", "list", "--host", "10.0.0.1"]).is_err());
    }

    /// 写入口撤了（ADR-0008）：建篮子、加东西、删东西都是改文件，这里不该再有命令。
    #[test]
    fn 只留读命令() {
        for gone in ["new", "add", "rm", "delete"] {
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
