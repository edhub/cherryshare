//! 网页那条路：给对方一个能看、能点着下载的页面。
//!
//! 页面是**磁盘上的静态文件**，不编进二进制——换页面不用重新出包。守护进程这边只做
//! 两件事：把篮子说成 JSON，以及把文件字节发出去。发文件复用 `listing::resolve`，
//! 跟 NFS 走的是同一套符号链接和越界检查（网页是第二个入口，不是新的权限模型）。
//!
//! URL 形状（第一段是篮子 id，也就是凭证）：
//!
//! ```text
//! GET /b/<篮子id>/             页面
//! GET /b/<篮子id>/_app/*       页面的静态资源
//! GET /b/<篮子id>/_api/list    篮子内容 + 挂载命令（JSON）
//! GET /b/<篮子id>/<路径>        文件字节（支持 Range，视频能拖进度）
//! ```
//!
//! **`_` 开头的段归守护进程。** 篮子里的 cherry 名字以 `_` 开头的，在网页上取不到；
//! NFS 那条路不受影响。
//!
//! 失败的三种说法分开：篮子不在、文件不在、网页文件没装。不许静默 404（ADR-0001）。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use cherry_core::link::{BasketLink, Host, MountOs};
use cherry_core::listing;
use cherry_core::{Basket, CherryKind, Service};

#[derive(Clone)]
struct App {
    svc: Service,
    /// 页面在哪。磁盘上的目录，不是编译进去的。
    web: Arc<PathBuf>,
}

pub async fn bind(port: u16) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind 0.0.0.0:{port} 失败"))
}

/// 绑好了就一直服务，直到进程结束。
pub async fn serve(listener: tokio::net::TcpListener, svc: Service, web: PathBuf) -> Result<()> {
    if !web.is_dir() {
        // 不静默：每个请求都会得到一句明确的话，启动日志里也留一条。
        tracing::error!(dir = %web.display(), "网页目录不在，页面打不开（跑一次 install.sh 或 `just install`）");
    }
    let app = Router::new()
        .route("/b/{id}/", get(page))
        .route("/b/{id}/{*rest}", get(any))
        .with_state(App {
            svc,
            web: Arc::new(web),
        });
    axum::serve(listener, app).await.context("HTTP 服务退出")?;
    Ok(())
}

async fn lookup(app: &App, id: &str) -> Option<Basket> {
    let id = BasketLink::parse_basket_id_hex(id)?;
    app.svc.basket_by_id(&id).await.ok()
}

/// `GET /b/<id>/`：页面。篮子得先存在，不然页面里的 `_api` 也没有意义。
async fn page(State(app): State<App>, Path(id): Path<String>) -> Response {
    if lookup(&app, &id).await.is_none() {
        return no_such_basket();
    }
    web_file(&app, "index.html", None, false).await
}

/// 页面之外的一切：静态资源、JSON、文件字节。
async fn any(
    State(app): State<App>,
    Path((id, rest)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let Some(basket) = lookup(&app, &id).await else {
        return no_such_basket();
    };
    let range = headers.get(header::RANGE).cloned();
    let head = method == Method::HEAD;

    match rest.split_once('/').unwrap_or((rest.as_str(), "")) {
        ("_api", "list") => list_json(&app, &basket, &headers, query.path).await,
        (seg, _) if seg.starts_with('_') => web_file(&app, &rest, range, head).await,
        _ => serve_file(&basket, &rest, range, head).await,
    }
}

/// 篮子内容，加上对方要用的挂载命令。
///
/// 命令跟 `cherry link` 同源：都是 `BasketLink` 生成的，不是另抄一份。
async fn list_json(
    app: &App,
    basket: &Basket,
    headers: &HeaderMap,
    sub: Option<String>,
) -> Response {
    let b = basket.clone();
    let dir = sub.clone().unwrap_or_default();
    let entries = match tokio::task::spawn_blocking(move || listing::list_dir(&b, &dir)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("篮子列不出来：{e:#}"),
            );
        }
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "列目录的任务挂了"),
    };

    // 挂载命令里的主机用对方访问时用的那个：他要是从 IP 进来的，给他名字没意义。
    let link = match request_host(headers) {
        Some(h) => BasketLink::new(h, basket.id),
        None => match app.svc.link(&basket.name, None).await {
            Ok(l) => l,
            Err(e) => {
                return plain(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("算不出链接：{e:#}"),
                );
            }
        },
    };
    let mounts = [(MountOs::Macos, "macOS"), (MountOs::Linux, "Linux")]
        .into_iter()
        .map(|(os, label)| {
            let mountpoint = match os {
                MountOs::Macos => link.macos_mountpoint(&basket.name),
                MountOs::Linux => link.linux_mountpoint(&basket.name),
            };
            MountJson {
                label,
                command: link.mount_command(os, &mountpoint),
                unmount: BasketLink::unmount_command(os, &mountpoint),
                mountpoint,
            }
        })
        .collect();

    Json(ListJson {
        name: basket.name.clone(),
        id: link.basket_id_hex(),
        link: link.to_string(),
        path: sub.unwrap_or_default(),
        mounts,
        cherries: entries
            .into_iter()
            .map(|e| CherryJson {
                name: e.name,
                kind: match e.kind {
                    CherryKind::File => "file",
                    CherryKind::Dir => "dir",
                },
                size: e.size,
                mtime: e.mtime,
            })
            .collect(),
    })
    .into_response()
}

/// 文件字节。路径校验跟 NFS 共用一套。
async fn serve_file(
    basket: &Basket,
    sub: &str,
    range: Option<HeaderValue>,
    head: bool,
) -> Response {
    let (b, s) = (basket.clone(), sub.to_owned());
    match tokio::task::spawn_blocking(move || listing::resolve(&b, &s)).await {
        // 目录不是文件。网页上要进目录是另一个请求（`_api/list?path=`），
        // 直接 GET 一个目录只可能是手敲 URL，那就说清楚。
        Ok(Ok((path, meta))) if meta.is_file() => serve_path(&path, range, head).await,
        Ok(Ok(_)) => plain(StatusCode::NOT_FOUND, "那是个目录，不是文件"),
        // 「不存在」和「非法」给同一个回答：别让人拿状态码探路。
        _ => no_such_file(),
    }
}

/// 页面自己的静态文件（`index.html`、`_app/…`）。这是磁盘上的路径，不走篮子的校验。
async fn web_file(app: &App, rel: &str, range: Option<HeaderValue>, head: bool) -> Response {
    if !app.web.is_dir() {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "网页文件没装：跑一下 install.sh（开发时 `just install`）",
        );
    }
    let path = app.web.join(rel);
    // 只许待在网页目录里面。
    if !path.starts_with(app.web.as_path()) || rel.split('/').any(|s| s == "..") {
        return no_such_file();
    }
    serve_path(&path, range, head).await
}

/// 交给 tower-http 的 `ServeFile`：Range、HEAD、Content-Type 它都管。
///
/// 路径必须是**已经校验过**的那个——它自己不认符号链接，也不认识篮子。
async fn serve_path(path: &PathBuf, range: Option<HeaderValue>, head: bool) -> Response {
    let mut send = Request::new(Body::empty());
    if let Some(v) = range {
        send.headers_mut().insert(header::RANGE, v);
    }
    if head {
        *send.method_mut() = Method::HEAD;
    }
    match ServeFile::new(path).oneshot(send).await {
        Ok(res) => res.map(Body::new),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "读文件失败"),
    }
}

/// 请求里的 `Host`，去掉端口。浏览器就是这么找到我们的，比猜本机名靠谱。
fn request_host(headers: &HeaderMap) -> Option<Host> {
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let host = if raw.starts_with('[') {
        // IPv6 是 [::1]:8080
        let close = raw.find(']')?;
        &raw[..=close]
    } else {
        raw.split(':').next()?
    };
    Host::parse(host).ok()
}

fn no_such_basket() -> Response {
    plain(StatusCode::NOT_FOUND, "没有这个篮子（链接可能已经作废）")
}

fn no_such_file() -> Response {
    plain(StatusCode::NOT_FOUND, "没有这个文件")
}

/// 一句话、能读懂。名字可能带尖括号之类，转义了再放进去。
fn plain(status: StatusCode, msg: &str) -> Response {
    let esc = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    (
        status,
        format!("<!doctype html><meta charset=\"utf-8\"><title>{esc}</title><p>{esc}</p>\n"),
    )
        .into_response()
}

#[derive(Serialize)]
struct ListJson {
    name: String,
    id: String,
    link: String,
    /// 这次列的是篮子内哪一层（空串是根）。
    path: String,
    mounts: Vec<MountJson>,
    cherries: Vec<CherryJson>,
}

/// `_api/list` 的参数：`?path=<篮子内相对路径>`。
#[derive(Deserialize)]
struct ListQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct MountJson {
    label: &'static str,
    mountpoint: String,
    command: String,
    unmount: String,
}

#[derive(Serialize)]
struct CherryJson {
    name: String,
    /// `file` / `dir`。页面自己翻译成人话。
    kind: &'static str,
    size: u64,
    /// Unix 秒。
    mtime: u64,
}
