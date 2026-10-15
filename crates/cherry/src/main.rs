//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004），全部操作都靠它。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。
//! 唯一一件非它不可的事：把相对路径按**自己的** cwd 变成绝对路径——cherryd 跑在
//! launchd 底下，cwd 是 `/`，它没法知道用户说的 `./docs` 是哪个目录。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cherry_control::{BasketView, CherryView, Client, LinkView, Request, Response};
use cherry_core::BasketLink;
use cherry_core::link::MountOs;
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
    /// 列出本机所有 basket
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
    /// 打出分享链接（连同对方要用的挂载命令）；不给篮子就每个篮子一条
    Link {
        /// 篮子名。不给我就把所有 basket 的链接列出来
        basket: Option<String>,
        /// 链接里填哪个地址。不给我就用本机名（`mbpmax.local` 这种，只在同一个局域网
        /// 里解析得了）；对方走路由或隧道时显式给一个 IP
        #[arg(long)]
        host: Option<String>,
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

    // “详情还是点名”只有客户端知道（服务端只回数据，ADR-0003）。
    let detail = matches!(cmd, Cmd::Show { basket: None });

    // 数据目录不做成选项：正常用的人就只有默认那一个，摆到帮助里是多余的。要开第二个
    // 实例（e2e）就设 `CHERRYSHARE_DATA_DIR`——`default_data_dir()` 会读。
    let data_dir = cherry_core::service::default_data_dir()?;
    let mut client = Client::connect(&data_dir).await?;
    match client.call(&to_request(cmd)?).await? {
        Response::Error(e) => bail!("{e}"),
        other => print(other, detail),
    }
    Ok(())
}

fn to_request(cmd: Cmd) -> Result<Request> {
    Ok(match cmd {
        Cmd::Status => Request::Status,
        Cmd::List => Request::ListBaskets,
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
        Cmd::Link { basket, host } => Request::Link { basket, host },
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

/// `detail`：`cherry list` 只是点名，`cherry show` 不带篮子要的是每个篮子的详情。
fn print(resp: Response, detail: bool) {
    match resp {
        Response::Error(_) => {}
        Response::Status {
            baskets,
            data_dir,
            nfs_port,
            nfs_up,
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
        }
        Response::Baskets(list) => {
            if list.is_empty() {
                println!("（还没有 basket。用 `cherry new <名字>` 建一个）");
            }
            for b in &list {
                // `cherry list` 只是点名；`cherry show` 不带篮子要的是每个篮子的详情。
                if detail {
                    print_basket(b);
                } else {
                    println!("{}  ({} 颗)", b.name, b.cherries.len());
                }
            }
        }
        Response::Basket(b) => print_basket(&b),
        // 每个篮子都给整段（挂载命令在里面）。不止一个时先报名字，不然光看链接分不清哪条是哪条。
        Response::Links(list) => {
            if list.is_empty() {
                println!("（还没有 basket。用 `cherry new <名字>` 建一个）");
            } else {
                for (i, l) in list.iter().enumerate() {
                    if i > 0 {
                        println!();
                    }
                    if list.len() > 1 {
                        println!("{}", l.name);
                    }
                    print_link(l);
                }
            }
        }
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

/// 一条链接的完整说明：链接本身、各系统的挂载命令、地址候选。
fn print_link(l: &LinkView) {
    println!("{}", l.link);
    println!();
    println!("把这条发给对方，在 terminal 中运行即可：");
    if let Ok(link) = l.link.parse::<BasketLink>() {
        println!();
        for (os, name, mnt) in [
            (
                MountOs::Macos,
                "macOS",
                format!("~/cherry/{}", link.mountpoint_leaf(&l.name)),
            ),
            (
                MountOs::Linux,
                "Linux",
                format!("/mnt/{}", link.mountpoint_leaf(&l.name)),
            ),
        ] {
            println!("  {name}");
            println!("    {}", link.mount_command(os, &mnt));
            println!("    卸掉：{}", BasketLink::unmount_command(os, &mnt));
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
    fn show_和_link_可以不带篮子() {
        let cli = Cli::try_parse_from(["cherry", "show"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Show { basket: None })));

        let cli = Cli::try_parse_from(["cherry", "link", "--host", "10.0.0.1"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::Link {
                basket: None,
                host: Some(_)
            })
        ));
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
