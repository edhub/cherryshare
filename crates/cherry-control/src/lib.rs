//! local socket 上的控制组消息（GUI / CLI）。见 ADR-0003。
//!
//! 这一层只搬字节：请求、事件、响应。所有语义判断都在 `cherry-core` 的
//! `Service` 里做一次，这里不重复。
//!
//! socket 权限 0600，内核强制只有同用户能连，所以不需要 token；没有端口，
//! 也就没有网络攻击面。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cherry_core::{
    Entry, Fetched, Notice, Service,
    proto::{read_frame, write_frame},
};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};

const SOCKET_FILE: &str = "cherryd.sock";

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
    /// 打出本机某个 basket 的分享链接。
    Link {
        basket: String,
    },
    /// 列对方篮子的一个目录。
    Ls {
        link: String,
        /// 篮子内的相对路径，空串表示篮子根。
        dir: String,
    },
    /// 从别人的篮子里取一个文件。
    Fetch {
        link: String,
        path: String,
        dest: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Info(String),
    Bytes { done: u64, total: Option<u64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Done,
    Status {
        endpoint: String,
        baskets: usize,
        data_dir: PathBuf,
    },
    Baskets(Vec<BasketView>),
    Basket(BasketView),
    Link(String),
    Listing(Vec<Entry>),
    Fetched(Fetched),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasketView {
    pub name: String,
    pub id: String,
    pub cherries: Vec<CherryView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CherryView {
    pub name: String,
    pub kind: String,
    pub source: String,
}

/// 服务端→客户端：先若干条事件，最后一条 Done。
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ServerMsg {
    Event(Event),
    Done(Response),
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

    /// 发一个请求，边收事件边等最终响应。
    pub async fn call(
        &mut self,
        req: &Request,
        mut on_event: impl FnMut(Event),
    ) -> Result<Response> {
        write_frame(&mut self.stream, req).await?;
        loop {
            let msg: ServerMsg = read_frame(&mut self.stream)
                .await?
                .context("cherryd 没说为什么就断了")?;
            match msg {
                ServerMsg::Event(e) => on_event(e),
                ServerMsg::Done(r) => return Ok(r),
            }
        }
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let mut emit = move |n: Notice| {
            let e = match n {
                Notice::Info(s) => Event::Info(s),
                Notice::Bytes { done, total } => Event::Bytes { done, total },
            };
            // 接收端没了也不影响操作本身。
            let _ = tx.send(e);
        };

        let fut = dispatch(&svc, &req, &mut emit);
        tokio::pin!(fut);

        // 操作在跑的同时把事件推出去，而不是憋到最后一起发。
        let resp = loop {
            tokio::select! {
                res = &mut fut => break res,
                Some(ev) = rx.recv() => write_frame(&mut wr, &ServerMsg::Event(ev)).await?,
            }
        };
        write_frame(&mut wr, &ServerMsg::Done(resp)).await?;
    }
    Ok(())
}

async fn dispatch(
    svc: &Service,
    req: &Request,
    emit: &mut (dyn FnMut(Notice) + Send),
) -> Response {
    match dispatch_inner(svc, req, emit).await {
        Ok(r) => r,
        // 失败一定要说清楚，不能静默。
        Err(e) => Response::Error(format!("{e:#}")),
    }
}

async fn dispatch_inner(
    svc: &Service,
    req: &Request,
    emit: &mut (dyn FnMut(Notice) + Send),
) -> Result<Response> {
    match req {
        Request::Status => {
            let baskets = svc.list_baskets().await;
            Ok(Response::Status {
                endpoint: svc.endpoint().id().to_string(),
                baskets: baskets.len(),
                data_dir: svc.data_dir().to_path_buf(),
            })
        }
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
        Request::Link { basket } => Ok(Response::Link(svc.link(basket).await?.to_string())),
        Request::Ls { link, dir } => {
            let link: cherry_core::BasketLink = link.parse().context("链接解析失败")?;
            Ok(Response::Listing(svc.list_dir(&link, dir).await?))
        }
        Request::Fetch { link, path, dest } => {
            let link: cherry_core::BasketLink = link.parse().context("链接解析失败")?;
            Ok(Response::Fetched(svc.fetch(&link, path, dest, emit).await?))
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
