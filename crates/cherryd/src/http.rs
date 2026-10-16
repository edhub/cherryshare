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
//! GET /b/<篮子id>/_api/size    一个目录总共多大（只 stat，有预算）
//! GET /b/<篮子id>/<路径>        文件字节（支持 Range，视频能拖进度）
//! GET /b/<篮子id>/<路径>?dl=zip 整个目录打包成一个 zip（流式的，不缓在内存里）
//! ```
//!
//! 打包在阻塞线程上做（读盘和 deflate 都是阻塞的），边压边往响应里写；因为不知道每个
//! 文件最终多大，大小和 CRC 写在条目尾部的 data descriptor 里（见 [`crate::tree`]）。
//! 同时最多打两个包：打包吃 CPU（一个核 145 MB/s 那个量级），开多了会把 NFS 那条主路
//! 挤到没核用。
//!
//! **`_` 开头的段归守护进程。** 篮子里的 cherry 名字以 `_` 开头的，在网页上取不到；
//! NFS 那条路不受影响。
//!
//! 失败的三种说法分开：篮子不在、文件不在、网页文件没装。不许静默 404（ADR-0001）。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use cherry_core::link::{BasketLink, Host, MountOs};
use cherry_core::listing;
use cherry_core::{Basket, CherryKind, Service};

use crate::tree;

/// 同时最多打几个包。见模块头上的说明。
const MAX_PACKS: usize = 2;
/// 同时最多算几个目录的大小。页面进一层就会对每个子目录问一次（只问看得见的），
/// 所以这里不能是 1（一行一行出数很慢），也不能不封顶（十几个可见行一起算就是十几个核）。
const MAX_SIZES: usize = 6;

#[derive(Clone)]
struct App {
    svc: Service,
    /// 页面在哪。磁盘上的目录，不是编译进去的。
    web: Arc<PathBuf>,
    packs: Arc<Semaphore>,
    sizes: Arc<Semaphore>,
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
            packs: Arc::new(Semaphore::new(MAX_PACKS)),
            sizes: Arc::new(Semaphore::new(MAX_SIZES)),
        });
    axum::serve(listener, app).await.context("HTTP 服务退出")?;
    Ok(())
}

async fn lookup(app: &App, id: &str) -> Option<Basket> {
    let id = BasketLink::parse_basket_id_hex(id)?;
    app.svc.basket_by_id(&id).await.ok()
}

/// `GET /b/<id>/`：页面。篮子得先存在，不然页面里的 `_api` 也没有意义。
///
/// 挂 `?dl=zip` 就是打整个篮子（`/b/<id>/` 是个目录，跟别的目录一样能打包；
/// 根这一层不走 `/b/{id}/{*rest}` 那条路由，所以在这里分流）。
async fn page(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<Params>,
    method: Method,
) -> Response {
    let Some(basket) = lookup(&app, &id).await else {
        return no_such_basket();
    };
    match query.dl.as_deref() {
        None => web_file(&app, "index.html", None, false).await,
        Some("zip") => zip_dir(&app, &basket, "", method == Method::HEAD).await,
        Some(other) => plain(
            StatusCode::BAD_REQUEST,
            &format!("`?dl={other}` 不认识，只认 `?dl=zip`"),
        ),
    }
}

/// 页面之外的一切：静态资源、JSON、文件字节、打包。
async fn any(
    State(app): State<App>,
    Path((id, rest)): Path<(String, String)>,
    Query(query): Query<Params>,
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
        ("_api", "size") => size_json(&app, &basket, query.path).await,
        (seg, _) if seg.starts_with('_') => web_file(&app, &rest, range, head).await,
        // 打包不是「另一个产品」，只是同一个目录的另一种拿法；参数写错要说出来，
        // 不能当没看见然后给人一个目录 404。
        _ => match query.dl.as_deref() {
            None => serve_file(&basket, &rest, range, head).await,
            Some("zip") => zip_dir(&app, &basket, &rest, head).await,
            Some(other) => plain(
                StatusCode::BAD_REQUEST,
                &format!("`?dl={other}` 不认识，只认 `?dl=zip`"),
            ),
        },
    }
}

/// 篮子内容，加上对方要用的挂载命令。
///
/// 命令由 `BasketLink` 生成，跟 NFS 那条路用的是同一份，不是另抄一条。
/// 这也是挂载命令唯一的出处——命令行里不再单打一份了。
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
        None => match app.svc.link(&basket.name, None, false).await {
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

/// 一个目录总共多大。只 stat 不读内容，而且有预算（见 `tree::size_of`）：跑这个的
/// 是**用户自己的电脑**，同一台还在给别人做 NFS，宁可算不完也不能算很久。
///
/// 超预算时数字不完整，所以照样回完整结构、但带 `stop`；页面两种原因都只说
/// 「文件太多」（用户理解不了「算不完」），日志里才分开。
async fn size_json(app: &App, basket: &Basket, sub: Option<String>) -> Response {
    let Some(sub) = sub else {
        return plain(StatusCode::BAD_REQUEST, "得给个 `?path=`");
    };
    let hint = sub.clone();
    let (b, s) = (basket.clone(), sub);
    let permit = match app.sizes.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "算大小的排队坏了"),
    };

    let done = tokio::task::spawn_blocking(move || -> Result<tree::Sized> {
        let _permit = permit;
        let (path, meta) = listing::resolve(&b, &s)?;
        if !meta.is_dir() {
            bail!("`{s}` 不是一个目录");
        }
        Ok(tree::size_of(&path))
    })
    .await;

    match done {
        Ok(Ok(sz)) => {
            // 界面上两种原因都说「文件太多」（用户理解不了「算不完」），但日志里分开记：
            // 真去查「为什么这个小文件夹也说太多」的时候，靠的就是这一行。
            if sz.stop != tree::Stop::Done {
                tracing::info!(path = %hint, stop = ?sz.stop, entries = sz.files + sz.dirs, "算大小提前停了");
            }
            Json(sz).into_response()
        }
        Ok(Err(e)) => plain(StatusCode::NOT_FOUND, &format!("算不了：{e:#}")),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "算大小的任务挂了"),
    }
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

/// `?dl=zip`：把一个目录打成 zip 发出去。
///
/// 路径校验跟 NFS、跟单个文件共享同一套（`listing::resolve`）——网页是第二个入口，
/// 不是新的权限模型。
///
/// 篮子根是特别的：它不是一个目录，是一排 cherry 并排，所以没有能 `resolve` 的单个路径，
/// 整篮子打包就是每颗 cherry 各自当一条顶层条目。
async fn zip_dir(app: &App, basket: &Basket, sub: &str, head: bool) -> Response {
    let (job, filename) = if sub.is_empty() {
        let roots = basket
            .cherries
            .iter()
            .map(|c| (c.name.clone(), c.source.clone()))
            .collect();
        (Job::Roots(roots), basket.name.clone())
    } else {
        let (b, s) = (basket.clone(), sub.to_owned());
        let dir = match tokio::task::spawn_blocking(move || listing::resolve(&b, &s)).await {
            Ok(Ok((path, meta))) if meta.is_dir() => path,
            // 文件不用打包，直接下就行（页面上也不会这么点）。
            Ok(Ok(_)) => return plain(StatusCode::BAD_REQUEST, "那是个文件，不用打包"),
            _ => return no_such_file(),
        };
        // 包内第一层目录名 = 对方在页面上看到的那个名字。
        let name = sub.rsplit('/').next().unwrap_or_default().to_owned();
        (
            Job::Dir {
                root: dir,
                wrapper: name.clone(),
            },
            name,
        )
    };

    let disposition = content_disposition(&filename);
    if head {
        // HEAD 只要头。真打一遍再扔掉，白白烧一遍 CPU。
        return zip_headers(disposition)
            .body(Body::empty())
            .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "组不出响应"));
    }

    let permit = match app.packs.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            // 要排队就明说：日志是错过那一刻唯一的记录。
            tracing::info!(what = %filename, "打包排在别人后面等着");
            match app.packs.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "打包的排队坏了"),
            }
        }
    };
    stream_zip(job, filename, disposition, permit)
}

/// 要打的东西。两种形状是因为篮子根和普通目录本来就不一样。
enum Job {
    /// 篮子里的一个目录：套一层同名目录。
    Dir { root: PathBuf, wrapper: String },
    /// 整个篮子：顶层条目并排。
    Roots(Vec<(String, PathBuf)>),
}

fn zip_headers(disposition: String) -> axum::http::response::Builder {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, disposition)
}

/// 字节从管道流给响应体。管道只有 256 KiB：对方下得慢，这边就少读点盘，
/// 不会把整个包堆在内存里。
fn stream_zip(
    job: Job,
    what: String,
    disposition: String,
    permit: OwnedSemaphorePermit,
) -> Response {
    const PIPE: usize = 256 * 1024;
    let (writer, reader) = tokio::io::duplex(PIPE);

    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        // `SyncIoBridge` 把阻塞这一头包成普普通通的 `Write`，写满了就等对方读。
        let mut out = tokio_util::io::SyncIoBridge::new(writer);
        let done = match &job {
            Job::Dir { root, wrapper } => tree::write_zip_dir(root, wrapper, &mut out),
            Job::Roots(roots) => tree::write_zip_roots(roots, &mut out),
        };
        match done {
            Ok(p) => tracing::info!(
                what = %what,
                files = p.files,
                mib = format!("{:.1}", p.mib()),
                skipped = p.skipped,
                secs = format!("{:.1}", started.elapsed().as_secs_f64()),
                "打包完了"
            ),
            // 对方半路关掉，或者写失败。日志留一条。
            Err(e) => tracing::warn!(what = %what, "包没打完: {e:#}"),
        }
        drop(permit);
    });

    zip_headers(disposition)
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(reader)))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "组不出响应"))
}

/// 附件名。中文名要走 RFC 5987 那种写法，不然对方看到一串问号；
/// 同时留一个能看的 ASCII 版给老东西。
fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_. ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    format!("attachment; filename=\"{ascii}.zip\"; filename*=UTF-8''{encoded}.zip")
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

/// 这次请求带的参数：`?path=<篮子内相对路径>`，以及 `?dl=zip`（打包整个目录）。
#[derive(Deserialize)]
struct Params {
    path: Option<String>,
    dl: Option<String>,
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
