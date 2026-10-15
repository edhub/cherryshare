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
use cherry_core::link::Host;

const SOCKET_FILE: &str = "cherryd.sock";

/// 一帧最大 16 MiB。篮子够大，但不该无限大。
const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_FILE)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Status,
    ListBaskets,
    ShowBasket {
        name: String,
    },
    CreateBasket {
        name: String,
    },
    AddCherry {
        basket: String,
        path: PathBuf,
    },
    RemoveCherry {
        basket: String,
        name: String,
    },
    /// 打出分享链接。`basket` 不给就是每个篮子一条，`host` 不给就用本机名（ADR-0002）。
    Link {
        basket: Option<String>,
        host: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Status {
        baskets: usize,
        data_dir: PathBuf,
        /// NFS 导出的端口。`nfs_up` 为假说明那个监听没起来（端口被占之类）。
        nfs_port: u16,
        nfs_up: bool,
    },
    Baskets(Vec<BasketView>),
    Basket(BasketView),
    Links(Vec<LinkView>),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasketView {
    pub name: String,
    pub id: String,
    pub cherries: Vec<CherryView>,
}

/// 一条链接，和它是哪个篮子的。
///
/// 链接里没有篮子名（它就是一个凭证，见 ADR-0002），所以列好几个的时候得把名字带上
/// 才知道哪条是哪条。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkView {
    pub name: String,
    pub link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CherryView {
    pub name: String,
    pub kind: String,
    pub source: String,
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
        Request::Status => Ok(Response::Status {
            baskets: svc.list_baskets().await.len(),
            data_dir: svc.data_dir().to_path_buf(),
            nfs_port: cherry_core::link::NFS_PORT,
            nfs_up: svc.nfs_up(),
        }),
        Request::ListBaskets => Ok(Response::Baskets(
            svc.list_baskets().await.iter().map(view).collect(),
        )),
        Request::ShowBasket { name } => Ok(Response::Basket(view(&svc.basket(name).await?))),
        Request::CreateBasket { name } => {
            let b = svc.create_basket(name).await?;
            Ok(Response::Basket(view(&b)))
        }
        Request::AddCherry { basket, path } => {
            svc.add_cherry(basket, path).await?;
            Ok(Response::Basket(view(&svc.basket(basket).await?)))
        }
        Request::RemoveCherry { basket, name } => {
            svc.remove_cherry(basket, name).await?;
            Ok(Response::Basket(view(&svc.basket(basket).await?)))
        }
        Request::Link { basket, host } => {
            let host = match host.as_deref() {
                Some(h) => Some(Host::parse(h)?),
                None => None,
            };
            // 不给篮子就是每个篮子一条。名字从 list_baskets 拿，链接本身没有篮子名。
            let names: Vec<String> = match basket {
                Some(name) => vec![name.clone()],
                None => svc
                    .list_baskets()
                    .await
                    .iter()
                    .map(|b| b.name.clone())
                    .collect(),
            };
            let mut out = Vec::with_capacity(names.len());
            for name in names {
                out.push(LinkView {
                    link: svc.link(&name, host.clone()).await?.to_string(),
                    name,
                });
            }
            Ok(Response::Links(out))
        }
    }
}

fn view(b: &cherry_core::Basket) -> BasketView {
    BasketView {
        name: b.name.clone(),
        id: hex::encode(b.id),
        cherries: b
            .cherries
            .iter()
            .map(|c| CherryView {
                name: c.name.clone(),
                kind: match c.kind {
                    cherry_core::CherryKind::File => "文件".into(),
                    cherry_core::CherryKind::Dir => "文件夹".into(),
                },
                source: c.source.display().to_string(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 帧往返() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::AddCherry {
                basket: "b".into(),
                path: "a/b.txt".into(),
            },
        )
        .await
        .unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let got: Request = read_frame(&mut cur).await.unwrap().unwrap();
        assert!(matches!(got, Request::AddCherry { .. }));
    }

    #[tokio::test]
    async fn 读到结尾返回_none() {
        let mut cur = std::io::Cursor::new(Vec::new());
        let got: Option<Request> = read_frame(&mut cur).await.unwrap();
        assert!(got.is_none());
    }
}
