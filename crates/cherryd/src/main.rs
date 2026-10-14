//! cherryd —— CherryShare 的服务。
//!
//! 三个东西独占一份：identity key / iroh endpoint（协议强制）、blobs store
//! （redb 排他锁，OS 强制）、basket 配置与索引缓存（设计选择）。见 ADR-0003。
//!
//! 只有一种起法：由系统拉起（macOS LaunchAgent / unix systemd），或者前台跑。

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CHERRY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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
