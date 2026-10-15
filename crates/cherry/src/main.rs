//! cherry —— CherryShare 的命令行界面。
//!
//! 是**完整的**操作界面，不是给脚本用的子集：v1 没有 GUI（ADR-0004），全部操作都靠它。
//! 它只是 local socket 上的一个消费者，语义判断一律不在这儿做（ADR-0003）。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cherry_control::{CherryView, Client, Request, Response};
use cherry_core::BasketLink;
use cherry_core::link::NFS_PORT;
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
        /// 文件或文件夹的路径
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
        /// 链接里填哪个地址。不给就问内核要（多网卡 / 有 VPN 时可能要显式给）
        #[arg(long)]
        host: Option<String>,
    },
    /// 挂上对方分享的篮子。不需要本机跑服务
    Mount {
        /// BasketLink
        link: String,
        /// 挂到哪儿。不给就是 ~/cherry/<id 前 8 位>
        mountpoint: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 挂载是纯客户端动作：对方的系统自带 NFS 客户端，本机不需要服务。
    if let Cmd::Mount { link, mountpoint } = cli.cmd {
        return mount(&link, mountpoint);
    }

    let data_dir = match cli.data_dir {
        Some(d) => d,
        None => cherry_core::service::default_data_dir()?,
    };
    let mut client = Client::connect(&data_dir).await?;
    match client.call(&to_request(cli.cmd)).await? {
        Response::Error(e) => bail!("{e}"),
        other => print(other),
    }
    Ok(())
}

fn to_request(cmd: Cmd) -> Request {
    match cmd {
        Cmd::Status => Request::Status,
        Cmd::List => Request::ListBaskets,
        Cmd::New { name } => Request::CreateBasket { name },
        Cmd::Show { basket } => Request::ShowBasket { name: basket },
        Cmd::Add { basket, path } => Request::AddCherry { basket, path },
        Cmd::Rm { basket, name } => Request::RemoveCherry { basket, name },
        Cmd::Link { basket, host } => Request::Link { basket, host },
        // main 里提前返回了。
        Cmd::Mount { .. } => unreachable!(),
    }
}

/// 把对方的篮子挂到本机。命令行就是链接里那些参数拼出来的（ADR-0002）。
fn mount(link: &str, mountpoint: Option<PathBuf>) -> Result<()> {
    let link: BasketLink = link.parse().context("链接看不懂")?;
    let mnt = match mountpoint {
        Some(p) => p,
        None => dirs::home_dir()
            .context("找不到 home 目录")?
            .join("cherry")
            .join(&link.basket_id_hex()[..8]),
    };
    std::fs::create_dir_all(&mnt).with_context(|| format!("建不出挂载点 {}", mnt.display()))?;

    let port = NFS_PORT.to_string();
    let opts = format!("nolocks,vers=3,tcp,port={port},mountport={port},soft,actimeo=5");
    let out = std::process::Command::new("mount_nfs")
        .arg("-o")
        .arg(&opts)
        .arg(link.mount_source())
        .arg(&mnt)
        .output()
        .context("跑不起 mount_nfs")?;
    if !out.status.success() {
        // "不允许静默失败"：把 mount_nfs 的原话带出来。
        bail!("挂载失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }

    println!("挂上了：{}", mnt.display());
    println!("（只读。想卸掉：`umount {}`）", mnt.display());
    Ok(())
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
            println!("把这条发给对方。");
            if let Ok(link) = l.parse::<BasketLink>() {
                let mnt = format!("~/cherry/{}", &link.basket_id_hex()[..8]);
                println!();
                println!("他装了 cherry：");
                println!("  cherry mount {l}");
                println!();
                println!("他没装，就在同一个局域网，粘这两行：");
                println!("  {}", link.mount_command(&mnt));
                println!("  umount {mnt}");

                // 自动挑的那个未必是对方能到的（有 VPN、有虚拟机网桥的时候），
                // 所以把其余候选也印出来。
                if let Ok(addrs) = cherry_core::link::lan_addrs()
                    && !addrs.others.is_empty()
                {
                    let list: Vec<String> = addrs.others.iter().map(|a| a.to_string()).collect();
                    println!(
                        "  （挑的是 {}。对方挂不上就换这几个试：{}）",
                        link.host,
                        list.join("  ")
                    );
                }
            }
        }
    }
}
