//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004），全部操作都靠它。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。
//! 唯一一件非它不可的事：把相对路径按**自己的** cwd 变成绝对路径——cherryd 跑在
//! launchd 底下，cwd 是 `/`，它没法知道用户说的 `./docs` 是哪个目录。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cherry_control::{BasketView, CherryView, Client, Request, Response};
use cherry_core::{BasketLink, link::Host};
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
    /// 建一个 basket，你可以往 basket 中添加你想要共享的文件、文件夹
    New {
        /// 篮子名
        name: String,
    },
    /// 看一个 basket 里有什么；不给篮子就所有篮子都看
    Show {
        /// 篮子名。不给我就把所有 basket 列出来
        basket: Option<String>,
    },
    /// 往篮子里放东西（放进去的只是引用，文件留在原处）
    Add {
        /// 篮子名
        basket: String,
        /// 文件或文件夹的路径。相对路径按你现在所在的目录算；篮子里记的永远是绝对路径
        path: PathBuf,
    },
    /// 从篮子里拿走一颗
    Rm {
        /// 篮子名
        basket: String,
        /// cherry 名
        name: String,
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
    match client.call(&to_request(cmd)?).await? {
        Response::Error(e) => bail!("{e}"),
        other => print(other),
    }
    Ok(())
}

fn to_request(cmd: Cmd) -> Result<Request> {
    Ok(match cmd {
        Cmd::Status => Request::Status,
        Cmd::List => Request::Links,
        Cmd::New { name } => Request::CreateBasket { name },
        Cmd::Show { basket } => match basket {
            Some(name) => Request::ShowBasket { name },
            None => Request::ListBaskets,
        },
        Cmd::Add { basket, path } => Request::AddCherry {
            basket,
            path: absolute(path)?,
        },
        Cmd::Rm { basket, name } => Request::RemoveCherry { basket, name },
    })
}

/// 把用户敲的路径变成绝对的、解析掉符号链接的路径。
///
/// 解析符号链接是因为服务那头本来就不对外提供符号链接（listing::servable），
/// 记下 resolve 之后的路径，两边说的才是同一个东西。
/// 顺带当存在性检查：篮子里存的路径得先活得过这一刻。报错时把按哪个目录算的说出来，
/// 因为相对路径出错时人第一个想知道的就是这个。
fn absolute(path: PathBuf) -> Result<PathBuf> {
    std::fs::canonicalize(&path).with_context(|| {
        if path.is_absolute() {
            format!("找不到 {}", path.display())
        } else {
            let cwd = std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "?".into());
            format!("找不到 {}（按当前目录 {cwd} 算的）", path.display())
        }
    })
}

/// `cherry show` 不带给篮子时也是每颗都给详情——反正它问的就是「里面有什么」。
fn print(resp: Response) {
    match resp {
        Response::Error(_) => {}
        Response::Status {
            baskets,
            data_dir,
            nfs_port,
            nfs_up,
            http_port,
            http_up,
        } => {
            println!("data       {}", data_dir.display());
            println!("baskets    {baskets}");
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
        Response::Baskets(list) => {
            if list.is_empty() {
                println!("（还没有 basket。用 `cherry new <名字>` 建一个）");
            }
            for b in &list {
                print_basket(b);
            }
        }
        Response::Basket(b) => print_basket(&b),
        // 一个篮子一小段：头上是名字和里面有几样东西，下面两条链接——本机名一条、IP 一条。
        // 两条一起摆出来是因为发送方判不出对方在不在同一个广播域（ADR-0002）：名字过不了
        // 路由和隧道，IP 这边一换 DHCP 地址就废了。URL 由它现场拼，这一步跟以前拼挂载命令
        // 是同一件事，纯粹是格式。
        // 挂载命令不在这儿打了：页面上有，而且那份是按对方自己的系统给的，比这边猜的准。
        Response::Links(list) => {
            if list.is_empty() {
                println!("（还没有 basket。用 `cherry new <名字>` 建一个）");
            }
            for (i, set) in list.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("{}（{} 项）", set.name, set.items);
                for raw in &set.links {
                    match raw.parse::<BasketLink>() {
                        Ok(link) => println!("  {}  {}", 地址种类(&link.host), link.web_url()),
                        // 解析不了就照原样打出来，别把服务给的东西吞掉。
                        Err(_) => println!("  {raw}"),
                    }
                }
            }
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

fn print_basket(b: &BasketView) {
    println!("{}  [{}]", b.name, b.id);
    if b.cherries.is_empty() {
        println!("  （空的。用 `cherry add {} <路径>` 放东西）", b.name);
    }
    for CherryView { name, kind, source } in &b.cherries {
        println!("  {name}  {kind}  ← {source}");
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

    /// 链接并进 `list` 之后，`cherry link` 就没了（挂载命令在页面上，按对方的系统给）。
    #[test]
    fn link_子命令没了() {
        assert!(Cli::try_parse_from(["cherry", "link"]).is_err());
    }

    #[test]
    fn add_还是得两个参数() {
        assert!(Cli::try_parse_from(["cherry", "add", "篮子"]).is_err());
    }

    /// 数据目录不给用户选（正常用不到），客户端就不认这个选项。
    #[test]
    fn 没有_data_dir_这个选项() {
        assert!(Cli::try_parse_from(["cherry", "--data-dir", "/tmp", "list"]).is_err());
    }
}
