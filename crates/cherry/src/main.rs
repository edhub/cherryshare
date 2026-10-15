//! cherry —— CherryShare 的命令行界面。
//!
//! 是完整的操作界面，不是给脚本用的子集：Unix 上没有 GUI，全部操作都得靠它。
//! 但它和 GUI 是兄弟不是父子：两个都只是 local socket 上的控制组消费者，
//! 语义判断一律不在这儿做（ADR-0003、ADR-0004）。

use std::path::PathBuf;

use anyhow::Result;
use cherry_control::{CherryView, Client, Event, Request, Response};
use cherry_core::CherryKind;
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
    /// 打出分享链接
    Link {
        /// 篮子名
        basket: String,
    },
    /// 列对方篮子里**一个目录**（ADR-0006：listing 的单位是一个目录，不是整棵树）
    Ls {
        /// BasketLink
        link: String,
        /// 篮子内的相对路径。不给就是篮子根。
        path: Option<String>,
    },
    /// 读对方一个文件的一段字节（默认从开头读 64 KiB）
    Cat {
        /// BasketLink
        link: String,
        /// 篮子内的相对路径
        path: String,
        /// 从哪个字节开始
        #[arg(long, default_value_t = 0)]
        start: u64,
        /// 读多少字节
        #[arg(long, default_value_t = 64 * 1024)]
        len: u64,
    },
    /// 从别人的篮子里挑一个文件下来
    Pick {
        /// BasketLink
        link: String,
        /// 篮子内的路径，从 open 的输出里抄
        path: String,
        /// 存到哪儿
        dest: PathBuf,
    },
}

// 这个 CLI 是个瘦客户端：真正的传输在 cherryd 里，这里只发请求、读进度。
// 所以不需要 cherryd 那个压 worker 数的处理（见 crates/cherryd 的 runtime()）。
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(d) => d,
        None => cherry_core::service::default_data_dir()?,
    };

    let mut client = Client::connect(&data_dir).await?;
    let req = to_request(cli.cmd);

    let resp = client
        .call(&req, |ev| {
            match ev {
                Event::Info(s) => println!("  {s}"),
                Event::Bytes { done, total } => print_progress(done, total),
            };
        })
        .await?;

    match resp {
        Response::Error(e) => anyhow::bail!("{e}"),
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
        Cmd::Link { basket } => Request::Link { basket },
        Cmd::Ls { link, path } => Request::Ls {
            link,
            dir: path.unwrap_or_default(),
        },
        Cmd::Cat {
            link,
            path,
            start,
            len,
        } => Request::Cat {
            link,
            path,
            start,
            len,
        },
        Cmd::Pick { link, path, dest } => Request::Fetch { link, path, dest },
    }
}

fn print(resp: Response) {
    match resp {
        Response::Done | Response::Error(_) => {}
        Response::Status {
            endpoint,
            baskets,
            data_dir,
        } => {
            println!("endpoint   {endpoint}");
            println!("baskets    {baskets}");
            println!("data       {}", data_dir.display());
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
            println!("把这条发给对方。它带完整的 endpoint 地址，所以同网段直连就行，");
            println!("不需要 relay 或 DNS。");
        }
        Response::Listing(entries) => {
            if entries.is_empty() {
                println!("（这个目录是空的）");
            }
            for e in entries {
                // 目录不给大小。它自己就是 0，而显示「0 B」是假的。
                match e.kind {
                    CherryKind::Dir => println!("{:>9}  {}/", "-", e.name),
                    CherryKind::File => println!("{:>9}  {}", human_size(e.size), e.name),
                }
            }
            println!();
            println!("进子目录：`cherry ls <链接> <路径>`");
            println!("取文件：  `cherry pick <链接> <路径> <存到哪>`");
        }
        Response::Fetched(f) => {
            println!(
                "下好了：{} → {}",
                human_size(f.bytes),
                f.path.display()
            );
        }
        Response::Bytes(bytes) => {
            use std::io::Write;
            // 管道断掉（`| head`）是正常的，不当错误。
            let _ = std::io::stdout().write_all(&bytes);
            let _ = std::io::stdout().flush();
        }
    }
}

/// 进度只往 stderr 之外的一行刷，不滚屏。
fn print_progress(done: u64, total: Option<u64>) {
    match total {
        Some(t) if t > 0 => eprint!("\r  {} / {}  ", human_size(done), human_size(t)),
        _ => eprint!("\r  {}  ", human_size(done)),
    }
    if done == total.unwrap_or(done) {
        eprintln!();
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
