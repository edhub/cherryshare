//! cherryd —— CherryShare 的服务。
//!
//! 两个东西独占一份：NFS 导出的监听端口（OS 强制），以及 basket 配置（设计选择）。
//! 见 ADR-0003。字节不经过这里——对方的内核直接读这台机器的磁盘（ADR-0007）。
//!
//! 只有一种起法：由系统拉起（macOS LaunchAgent / unix systemd），或者前台跑。
//!
//! **tokio worker 数是刻意压低的一个显式选择**，理由见 [`runtime`]。

use std::path::PathBuf;

use anyhow::{Context, Result};
use cherry_core::Service;
use clap::Parser;
use tokio::net::UnixListener;

mod nfs;

#[derive(Debug, Parser)]
#[command(name = "cherryd", version, about = "CherryShare 的服务")]
struct Args {
    /// 数据目录。默认取 $CHERRYSHARE_DATA_DIR，再退到系统数据目录。
    #[arg(long, env = "CHERRYSHARE_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

/// cherryd 的 tokio worker 数。默认 **1**，不是 `num_cpus`。
///
/// **现在这条负载是“很轻的 RPC 解析 + 很多阻塞文件 I/O”。** 后者全部在
/// `spawn_blocking` 里（见 `nfs.rs`），所以一个 worker 足够：实测 NFS 冷读
/// 747 MB/s 就是在 worker=1 下跑的（而且服务端还是 debug 版）。
///
/// 历史上这个默认值是从另一条传输量出来的——当时发现“worker 越多越差，
/// 而且成本随并发退化”（4 并发时默认的 ~10 个 worker 烧 22.17 CPU 秒处理
/// 200 MiB，串行跑 4 次只要 8.9 秒）。那条传输（iroh）已经删了，数字也不再直接
/// 适用，记在 `docs/performance.md`。**但也没有任何理由往上调**：这台机器上
/// 真正花钱的活都在阻塞线程里。
///
/// **代价与前提。** 1 个 worker 意味着任何阻塞调用都会卡住整个服务，包括正在跑的
/// basket 管理请求。已知的阻塞点都进了 `spawn_blocking`；**没审计过的是 `nfsserve`
/// 内部**——如果哪天出现“服务整体卡住”的怪象，先试 `CHERRY_WORKERS=4` 排除这一条。
fn runtime() -> Result<tokio::runtime::Runtime> {
    let workers = std::env::var("CHERRY_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .context("建 tokio 运行时")
}

fn main() -> Result<()> {
    use std::io::IsTerminal;
    // 服务常驻的时候 stderr 是个文件（launchd 写 ~/Library/Logs/cherryd.log）。
    // 日志是「源没了」这类事件唯一的记录，不能往里灌转义码，不然 grep 出来没法看。
    tracing_subscriber::fmt()
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CHERRY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    runtime()?.block_on(run())
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let data_dir = match args.data_dir {
        Some(d) => d,
        None => cherry_core::service::default_data_dir()?,
    };

    let svc = Service::start(data_dir.clone()).await?;

    let sock = cherry_control::socket_path(&data_dir);
    // 上一次没清干净的 socket 文件会让 bind 失败。
    let _ = std::fs::remove_file(&sock);
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("bind {} 失败", sock.display()))?;
    restrict_to_owner(&sock)?;

    // 局域网那条路：把篮子当只读 NFS 导出，对方的系统自带客户端就能挂。
    //
    // 端口被占**不算致命**：本地那部分（basket 管理、给链接）还能用。但绝不能静默——
    // 那会变成“对方挂不上，而这里什么都没说”。
    let nfs = match nfs::serve(svc.clone(), cherry_core::link::NFS_PORT).await {
        Ok(l) => {
            svc.set_nfs_up(true);
            Some(l)
        }
        Err(cause) => {
            tracing::error!("NFS 导出起不来（{cause}）：局域网挂载这条路不可用");
            None
        }
    };

    println!("cherryd");
    println!("  data       {}", data_dir.display());
    println!("  socket     {}", sock.display());
    match &nfs {
        Some(_) => println!("  nfs        0.0.0.0:{}", cherry_core::link::NFS_PORT),
        None => println!("  nfs        没起来（局域网挂载这条路不可用）"),
    }
    println!("  等待中（Ctrl-C 退出）");

    tokio::select! {
        r = cherry_control::serve(listener, svc.clone()) => r?,
        r = nfs_forever(nfs) => r?,
        _ = tokio::signal::ctrl_c() => println!("收到 Ctrl-C，收摊"),
    }

    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// 没起来就永远挂着，让 `select!` 少一个分支。
async fn nfs_forever(
    l: Option<nfsserve::tcp::NFSTcpListener<nfs::BasketNfs>>,
) -> std::io::Result<()> {
    use nfsserve::tcp::NFSTcp;
    match l {
        Some(l) => l.handle_forever().await,
        None => std::future::pending().await,
    }
}

/// socket 权限 0600：内核强制只有同用户能连，所以不需要 token（ADR-0003）。
fn restrict_to_owner(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
