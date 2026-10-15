//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004），全部操作都靠它。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。
//! 唯一一件非它不可的事：把相对路径按**自己的** cwd 变成绝对路径——cherryd 跑在
//! launchd 底下，cwd 是 `/`，它没法知道用户说的 `./docs` 是哪个目录。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cherry_control::{CherryView, Client, Request, Response};
use cherry_core::BasketLink;
use cherry_core::link::MountOs;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cherry", version, about = "CherryShare —— 把一篮樱桃递给别人")]
struct Cli {
    #[arg(long, env = "CHERRYSHARE_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// 服务状态
    Status,
    /// 列出本机所有 basket
    List,
    /// 建一个 basket
    New {
        /// 篮子名
        name: String,
    },
    /// 看一个 basket 里有什么
    Show {
        /// 篮子名
        basket: String,
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
    /// 打出分享链接（连同对方要用的挂载命令）
    Link {
        /// 篮子名
        basket: String,
        /// 链接里填哪个地址。不给我就用本机名（`mbpmax.local` 这种，只在同一个局域网
        /// 里解析得了）；对方走路由或隧道时显式给一个 IP
        #[arg(long)]
        host: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(d) => d,
        None => cherry_core::service::default_data_dir()?,
    };
    let mut client = Client::connect(&data_dir).await?;
    match client.call(&to_request(cli.cmd)?).await? {
        Response::Error(e) => bail!("{e}"),
        other => print(other),
    }
    Ok(())
}

fn to_request(cmd: Cmd) -> Result<Request> {
    Ok(match cmd {
        Cmd::Status => Request::Status,
        Cmd::List => Request::ListBaskets,
        Cmd::New { name } => Request::CreateBasket { name },
        Cmd::Show { basket } => Request::ShowBasket { name: basket },
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

fn print(resp: Response) {
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
            for b in list {
                println!("{}  ({} 颗)", b.name, b.cherries.len());
            }
        }
        Response::Basket(b) => {
            println!("{}  [{}]", b.name, b.id);
            if b.cherries.is_empty() {
                println!("  （空的。用 `cherry add {} <路径>` 放东西）", b.name);
            }
            for CherryView { name, kind, source } in b.cherries {
                println!("  {name}  {kind}  ← {source}");
            }
        }
        Response::Link(l) => {
            println!("{l}");
            println!();
            println!("把这条发给对方。他什么都不用装，在同一个局域网里粘对应的一行：");
            if let Ok(link) = l.parse::<BasketLink>() {
                println!();
                for (os, name, mnt) in [
                    (
                        MountOs::Macos,
                        "macOS",
                        format!("~/cherry/{}", &link.basket_id_hex()[..8]),
                    ),
                    (MountOs::Linux, "Linux", "/mnt/cherry".to_owned()),
                ] {
                    println!("  {name}");
                    println!("    {}", link.mount_command(os, &mnt));
                    println!("    卸掉：{}", BasketLink::unmount_command(os, &mnt));
                }

                // 链接默认是本机名，而名字只在同一个广播域里解析得了（mDNS 是组播，
                // 过不了路由，也过不了隧道）。所以把地址候选印出来：对方走别处就换 IP。
                if let Ok(addrs) = cherry_core::link::lan_addrs() {
                    let mut list: Vec<String> = vec![addrs.preferred.to_string()];
                    list.extend(addrs.others.iter().map(|a| a.to_string()));
                    println!(
                        "  对方走路由或隧道就换成 IP（挑的是 {}）：",
                        addrs.preferred
                    );
                    println!(
                        "    cherry link <篮子名> --host <其中一个>   {}",
                        list.join("  ")
                    );
                }
            }
        }
    }
}
