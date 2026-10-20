//! local socket 上的控制组消息。见 ADR-0003。
//!
//! 这一层只搬字节：请求和响应。所有语义判断都在 `cherry-core` 的 `Service` 里做一次，
//! 这里不重复。
//!
//! **这一层是"我们的"唯一的线上协议**（帧格式 + 这两个 enum）。别的传输都是内核的事：
//! NFS 那条路由系统自带的客户端讲（ADR-0007）。
//!
//! socket 权限 0600，内核强制只有同用户能连，所以不需要 token；客户端这一侧没有端口，
//! 也就没有网络攻击面。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use cherry_core::Service;

const SOCKET_FILE: &str = "cherryd.sock";

/// 一帧最大 16 MiB。篮子够大，但不该无限大。
const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_FILE)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Status,
    /// 每个篮子的详情（`cherry show` 不给名字的时候）。
    ListBaskets,
    ShowBasket {
        name: String,
    },
    /// 每个篮子每个地址一条网页链接——本机名一条，IP 一条（见
    /// `cherry_core::Service::links`）。挂载命令不在这儿给，它在页面上，
    /// 而且是按对方自己的系统给的（ADR-0002）。
    Links,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Status {
        baskets: usize,
        /// 数据目录里读不进来的文件有几个。
        broken: usize,
        data_dir: PathBuf,
        /// NFS 导出的端口。`nfs_up` 为假说明那个监听没起来（端口被占之类）。
        nfs_port: u16,
        nfs_up: bool,
        /// 网页那条路的端口。`http_up` 为假说明页面点不开。
        http_port: u16,
        http_up: bool,
    },
    Baskets {
        /// 没东西可看的时候得告诉人去哪儿写文件。
        dir: PathBuf,
        ok: Vec<BasketView>,
        broken: Vec<BrokenView>,
    },
    Basket(BasketView),
    Links {
        dir: PathBuf,
        sets: Vec<LinkSet>,
        broken: Vec<BrokenView>,
    },
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasketView {
    pub name: String,
    pub id: String,
    /// 定义这个篮子的文件。要改篮子就是去改它。
    pub file: String,
    /// 文件原文。`cherry show` 就是把它打出来——文件是唯一真相，没必要再翻译一遍。
    pub content: String,
    /// 文件里写着、但现在读不到的路径（带原因）。这是唯一一处「算出来的东西」：
    /// 有人看的时候不许可说假的。
    pub missing: Vec<String>,
}

/// 数据目录里读不进来的那个文件，以及为什么。它没有 id，所以别人的链接指不到它，
/// 只在 `cherry list` / `show` 里露面。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenView {
    pub name: String,
    pub file: String,
    pub error: String,
}

/// 一个篮子和它能发出去的几条链接。
///
/// 链接里没有篮子名（它就是一个凭证，见 ADR-0002），所以列好几个篮子的时候得把名字带上
/// 才知道哪条是哪条。`links` 按该递给对方的先后来排：本机名在前，IP 在后。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSet {
    pub name: String,
    /// 要改这个篮子就去改这个文件。
    pub file: String,
    /// 篮子里有几颗 cherry。列篮子的时候顺带让人看出它是不是空的。
    pub items: usize,
    pub links: Vec<String>,
}

// ---- 帧：长度前缀 + postcard ----

async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = postcard::to_stdvec(msg).context("序列化失败")?;
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// 对端干净关闭时返回 `None`。
async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = match r.read_u32().await {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("读帧长度失败"),
    };
    if len > MAX_FRAME {
        bail!("帧太大：{len} 字节");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await.context("读帧内容失败")?;
    Ok(Some(postcard::from_bytes(&buf).context("反序列化失败")?))
}

// ---- 客户端 ----

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub async fn connect(data_dir: &Path) -> Result<Self> {
        let path = socket_path(data_dir);
        let stream = UnixStream::connect(&path).await.with_context(|| {
            format!(
                "连不上 {} —— cherryd 在跑吗？（`cherryd` 前台起，或用系统服务拉起）",
                path.display()
            )
        })?;
        Ok(Self { stream })
    }

    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        write_frame(&mut self.stream, req).await?;
        read_frame::<_, Response>(&mut self.stream)
            .await?
            .context("cherryd 没说为什么就断了")
    }
}

// ---- 服务端 ----

pub async fn serve(listener: UnixListener, svc: Service) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await.context("accept 失败")?;
        let svc = svc.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, svc).await {
                tracing::warn!(error = format!("{e:#}"), "控制连接结束");
            }
        });
    }
}

async fn handle_conn(stream: UnixStream, svc: Service) -> Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    while let Some(req) = read_frame::<_, Request>(&mut rd).await? {
        let resp = dispatch(&svc, &req).await;
        write_frame(&mut wr, &resp).await?;
    }
    Ok(())
}

async fn dispatch(svc: &Service, req: &Request) -> Response {
    match dispatch_inner(svc, req).await {
        Ok(r) => r,
        // 失败一定要说清楚，不能静默。
        Err(e) => Response::Error(format!("{e:#}")),
    }
}

async fn dispatch_inner(svc: &Service, req: &Request) -> Result<Response> {
    match req {
        Request::Status => {
            let all = svc.baskets().await;
            Ok(Response::Status {
                baskets: all.ok.len(),
                broken: all.broken.len(),
                data_dir: svc.data_dir().to_path_buf(),
                nfs_port: cherry_core::link::NFS_PORT,
                nfs_up: svc.nfs_up(),
                http_port: cherry_core::link::http_port(),
                http_up: svc.http_up(),
            })
        }
        Request::ListBaskets => {
            let all = svc.baskets().await;
            Ok(Response::Baskets {
                dir: svc.data_dir().to_path_buf(),
                ok: all.ok.iter().map(view).collect(),
                broken: all.broken.iter().map(broken_view).collect(),
            })
        }
        Request::ShowBasket { name } => Ok(Response::Basket(view(&svc.basket(name).await?))),
        Request::Links => {
            let all = svc.baskets().await;
            let mut sets = Vec::with_capacity(all.ok.len());
            for b in &all.ok {
                sets.push(LinkSet {
                    name: b.name.clone(),
                    file: b.file.display().to_string(),
                    items: b.cherries.len(),
                    links: svc
                        .links_of(b)?
                        .into_iter()
                        .map(|l| l.to_string())
                        .collect(),
                });
            }
            Ok(Response::Links {
                dir: svc.data_dir().to_path_buf(),
                sets,
                broken: all.broken.iter().map(broken_view).collect(),
            })
        }
    }
}

/// 一个篮子交给用户看的样子：文件原文 + 哪几行现在读不到。
///
/// 不再自己拼一份「篮子里有什么」——那是文件的事（ADR-0008）。要算的只有死活：
/// 磁盘上的事文件里没有，而「有人看的时候不许可说假的」。
fn view(b: &cherry_core::Basket) -> BasketView {
    BasketView {
        name: b.name.clone(),
        id: hex::encode(b.id),
        file: b.file.display().to_string(),
        content: std::fs::read_to_string(&b.file).unwrap_or_default(),
        missing: b
            .cherries
            .iter()
            .filter_map(|c| {
                let why = match std::fs::symlink_metadata(&c.source) {
                    Ok(meta) if cherry_core::listing::servable(&meta).is_some() => return None,
                    Ok(_) => "不是普通文件也不是真目录，不共享".to_owned(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => "源不在了".to_owned(),
                    Err(e) => format!("读不到元数据: {e}"),
                };
                Some(format!("{}（{why}）", c.source.display()))
            })
            .collect(),
    }
}

fn broken_view(b: &cherry_core::service::Broken) -> BrokenView {
    BrokenView {
        name: b.name.clone(),
        file: b.file.display().to_string(),
        error: b.error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 帧往返() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::ShowBasket { name: "b".into() })
            .await
            .unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let got: Request = read_frame(&mut cur).await.unwrap().unwrap();
        assert!(matches!(got, Request::ShowBasket { .. }));
    }

    #[tokio::test]
    async fn 读到结尾返回_none() {
        let mut cur = std::io::Cursor::new(Vec::new());
        let got: Option<Request> = read_frame(&mut cur).await.unwrap();
        assert!(got.is_none());
    }
}
