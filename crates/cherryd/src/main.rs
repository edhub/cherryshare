//! cherryd —— CherryShare 的服务。
//!
//! 三个东西独占一份：identity key / iroh endpoint（协议强制）、blobs store
//! （redb 排他锁，OS 强制）、basket 配置与索引缓存（设计选择）。见 ADR-0003。
//!
//! 只有一种起法：由系统拉起（macOS LaunchAgent / unix systemd），或者前台跑。
//!
//! **tokio worker 数是刻意压低的一个显式选择**，理由见 [`runtime`]。

use std::path::PathBuf;

use anyhow::{Context, Result};
use cherry_core::Service;
use clap::Parser;
use tokio::net::UnixListener;

#[derive(Debug, Parser)]
#[command(name = "cherryd", version, about = "CherryShare 的服务")]
struct Args {
    /// 数据目录。默认取 $CHERRYSHARE_DATA_DIR，再退到系统数据目录。
    #[arg(long, env = "CHERRYSHARE_DATA_DIR")]
    data_dir: Option<PathBuf>,
}

/// cherryd 的 tokio worker 数。默认 **1**，不是 `num_cpus`。
///
/// 实测（macOS loopback，200 MiB，两端合计 CPU）——单位 ns/字节：
///
/// ```text
/// workers      1 个下载   2 个并发   4 个并发
/// 1              27.5      26.8       26.8
/// 2              35.3      49.4       49.9
/// ~10（默认）     42.3      71.2      105.7
/// ```
///
/// **worker 越多越差，而且成本随并发退化。** 这个负载是几条热的单线程状态机
/// （endpoint driver、每条连接的 driver、blob 传输），摊到多个核上只会付
/// 任务迁移 + 跳核唤醒 + 工作窃取；少两个 worker 反而让它们在同一核上跑，缓存是热的。
///
/// **顺带纠正一个直觉：1 个 worker 不会串行化。** 并发上去吞吐反而涨
/// （57.3 → 65.0 → 68.0 MB/s），而每字节成本完全平坦（~27 ns）。
///
/// **代价与前提。** 1 个 worker 意味着任何一个阻塞调用都会卡住整个守护进程。
/// 已知的阻塞点已经处理：`listing::list_dir`（同步目录遍历）丢进了 `spawn_blocking`；
/// iroh-blobs 的 store 有自己的线程（`iroh-blob-store-0`）做文件 I/O。
/// **没审计过的是 iroh/noq 内部**——如果哪天出现“守护进程整体卡住”的怪象，
/// 先试 `CHERRY_WORKERS=4` 排除这一条。
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
    tracing_subscriber::fmt()
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
    // 协议服务：我们的 ALPN + iroh-blobs 的原生 ALPN。
    let router = svc.spawn_router()?;

    let sock = cherry_control::socket_path(&data_dir);
    // 上一次没清干净的 socket 文件会让 bind 失败。
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("bind {} 失败", sock.display()))?;
    restrict_to_owner(&sock)?;

    // 这台机器的地址就是别人要连的东西。地址变了要说出来。
    println!("cherryd");
    println!("  endpoint   {}", svc.endpoint().id());
    println!("  addr       {:?}", svc.endpoint().addr());
    println!("  data       {}", data_dir.display());
    println!("  socket     {}", sock.display());
    println!("  等待中（Ctrl-C 退出）");

    tokio::select! {
        r = cherry_control::serve(listener, svc.clone()) => r?,
        _ = tokio::signal::ctrl_c() => println!("收到 Ctrl-C，收摊"),
    }

    router.shutdown().await?;
    svc.store().shutdown().await?;
    let _ = std::fs::remove_file(&sock);
    Ok(())
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
