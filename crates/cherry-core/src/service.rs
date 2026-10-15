//! 服务：所有语义判断的唯一落点。见 ADR-0003。
//!
//! 客户端只收集输入、呈现输出。合法性、路径规范化、冲突、状态机在这里做一次。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, protocol::Router};
use iroh_blobs::{
    BlobsProtocol, Hash,
    api::{Store, remote::GetProgressItem},
    store::fs::FsStore,
};
use futures_lite::StreamExt;
use tokio::sync::RwLock;

use crate::{
    basket::Basket,
    endpoint,
    index::Index,
    link::BasketLink,
    listing::Entry,
    proto::{ALPN, WireRequest, WireResponse, read_frame, write_frame},
    provider::Provider,
};

/// 进度通知。客户端把它变成自己要显示的东西；core 不认识「UI」。
#[derive(Debug, Clone)]
pub enum Notice {
    Info(String),
    /// `done` 是**本次请求**读到的字节数，不是累计进度。
    /// 所以断点续传时它会从 0 重新开始，不是从已经有的那部分接着数——
    /// 界面若直接拿它当「已完成 x/y」会误导。待修。
    Bytes { done: u64, total: Option<u64> },
}

/// 下载结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fetched {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    endpoint: Endpoint,
    store: Store,
    /// 唯一的持久状态里，这份是「设计选择」而不是「OS 强制」（ADR-0003）。
    baskets: RwLock<Vec<Basket>>,
    /// 索引缓存（ADR-0002 要求）。也是「设计选择」那一类：坏了只丢缓存，不丢数据。
    index: RwLock<Index>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("endpoint", &self.inner.endpoint.id())
            .field("data_dir", &self.inner.data_dir)
            .field("baskets", &self.inner.baskets)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// 起服务：拿 identity key、开 blobs store、读 basket 配置。
    pub async fn start(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("建不出数据目录 {}", data_dir.display()))?;

        let secret = load_or_create_identity(&data_dir)?;
        let ep = endpoint::bind(secret).await?;

        let blobs_dir = data_dir.join("blobs");
        std::fs::create_dir_all(&blobs_dir)?;
        // redb 排他锁在这里：同一台机器上只能有一个服务实例（ADR-0003）。
        let fs = FsStore::load(&blobs_dir)
            .await
            .context("打开 blobs store 失败（是不是已经有 cherryd 在跑？）")?;
        let store: Store = fs.into();

        let baskets = load_baskets(&data_dir)?;
        // 缓存读不了不该让服务起不来——它只是缓存。
        let index = Index::load(&data_dir).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "索引缓存读不了，当作空的");
            Index::default()
        });
        tracing::info!(
            endpoint = %ep.id(),
            baskets = baskets.len(),
            "cherryd 起来了"
        );

        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                endpoint: ep,
                store,
                baskets: RwLock::new(baskets),
                index: RwLock::new(index),
            }),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// 我们的 ALPN + iroh-blobs 的原生 ALPN。
    ///
    /// 后者是必需的：`GetFile` 只回 hash，字节由查看者按 hash 自己取。
    pub fn spawn_router(&self) -> Result<Router> {
        let provider = Provider::new(self.clone());
        let blobs = BlobsProtocol::new(&self.inner.store, None);
        Ok(Router::builder(self.inner.endpoint.clone())
            .accept(ALPN, provider)
            .accept(iroh_blobs::ALPN, blobs)
            .spawn())
    }

    // ---- 创建者侧 ----

    pub async fn create_basket(&self, name: &str) -> Result<Basket> {
        let mut baskets = self.inner.baskets.write().await;
        if baskets.iter().any(|b| b.name == name) {
            bail!("已经有一个叫 `{name}` 的 basket 了");
        }
        let b = Basket::new(name)?;
        baskets.push(b.clone());
        drop(baskets);
        self.persist().await?;
        Ok(b)
    }

    pub async fn add_cherry(&self, basket: &str, source: &Path) -> Result<crate::Cherry> {
        let mut baskets = self.inner.baskets.write().await;
        let b = baskets
            .iter_mut()
            .find(|b| b.name == basket)
            .with_context(|| format!("没有叫 `{basket}` 的 basket"))?;
        let cherry = b.add(source)?;
        drop(baskets);
        self.persist().await?;
        Ok(cherry)
    }

    pub async fn remove_cherry(&self, basket: &str, name: &str) -> Result<crate::Cherry> {
        let mut baskets = self.inner.baskets.write().await;
        let b = baskets
            .iter_mut()
            .find(|b| b.name == basket)
            .with_context(|| format!("没有叫 `{basket}` 的 basket"))?;
        let cherry = b.remove(name)?;
        drop(baskets);
        self.persist().await?;
        Ok(cherry)
    }

    pub async fn list_baskets(&self) -> Vec<Basket> {
        self.inner.baskets.read().await.clone()
    }

    pub async fn basket(&self, name: &str) -> Result<Basket> {
        self.inner
            .baskets
            .read()
            .await
            .iter()
            .find(|b| b.name == name)
            .cloned()
            .with_context(|| format!("没有叫 `{name}` 的 basket"))
    }

    pub async fn basket_by_id(&self, id: &[u8; 16]) -> Result<Basket> {
        self.inner
            .baskets
            .read()
            .await
            .iter()
            .find(|b| &b.id == id)
            .cloned()
            // 不泄漏「这个 id 存在但你没权限」这种区别。
            .context("这个 basket 不在我这儿")
    }

    /// 本机这个 basket 的链接。带当前 endpoint 地址。
    pub async fn link(&self, basket: &str) -> Result<BasketLink> {
        let b = self.basket(basket).await?;
        Ok(BasketLink::new(self.inner.endpoint.addr(), b.id))
    }

    // ---- 查看者侧 ----

    /// 列对方篮子里**一个目录**。快：对面只做一次 `readdir` + 每个 entry 一次 `stat`。
    pub async fn list_dir(&self, link: &BasketLink, dir: &str) -> Result<Vec<Entry>> {
        let conn = self.connect(link).await?;
        let (mut send, mut recv) = conn.open_bi().await.context("开流失败")?;
        write_frame(
            &mut send,
            &WireRequest::ListDir {
                basket_id: link.basket_id,
                dir: dir.to_owned(),
            },
        )
        .await?;
        send.finish().context("关流失败")?;

        match read_frame::<_, WireResponse>(&mut recv).await? {
            Some(WireResponse::Listing { entries }) => Ok(entries),
            Some(WireResponse::Error { message }) => bail!("创建者那边：{message}"),
            Some(other) => bail!("意外的响应：{other:?}"),
            None => bail!("创建者没回话就断了"),
        }
    }

    /// 取一个文件：先问 hash（对面现算），再按 hash 走 iroh-blobs 取字节。
    ///
    /// **缺口：这次取字节的生存期挂在调用方这个 future 上。** GUI / CLI 是通过 local
    /// socket 的请求处理任务调进来的，客户端一退出，任务被 drop，取到一半就断。
    /// 实测（客户端退出后看 store 增长）只多活了半个进度步长 ≈ 5.7MB。
    ///
    /// 注意这**不是** ADR-0003 的承诺——那条说的是「Cmd+Q 不影响**别人下载**」，
    /// 讲的是创建者一侧。ADR-0003 对数据一侧的要求在 mount 那节：**服务侧做块级
    /// 缓存与预读、协议支持流式 range 读**（原话：「这不是优化」）。
    ///
    /// 所以正确的形状不是「把下载变成独立 job」——那是按文件切，而 mount 要的是按块、
    /// 随机、可重复地读。应该是**服务侧一份块级缓存加一个填充它的取数循环**：
    /// mount 从缓存里读，`pick` 变成「填满再拷出去」。填充循环在服务里，所以
    /// 「取数不依赖客户端存活」是推论而不是特性。
    pub async fn fetch(
        &self,
        link: &BasketLink,
        path: &str,
        dest: &Path,
        notice: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<Fetched> {
        notice(Notice::Info(format!("问创建者要 `{path}` 的 hash…")));

        let conn = self.connect(link).await?;
        let (mut send, mut recv) = conn.open_bi().await.context("开流失败")?;
        write_frame(
            &mut send,
            &WireRequest::GetFile {
                basket_id: link.basket_id,
                path: path.to_owned(),
            },
        )
        .await?;
        send.finish().context("关流失败")?;

        let (hash, size) = match read_frame::<_, WireResponse>(&mut recv).await? {
            Some(WireResponse::File { hash, size }) => (hash, size),
            Some(WireResponse::Error { message }) => bail!("创建者那边：{message}"),
            Some(other) => bail!("意外的响应：{other:?}"),
            None => bail!("创建者没回话就断了"),
        };
        notice(Notice::Info(format!(
            "创建者准备好了（{size} 字节），开始取字节…"
        )));

        self.download_blob(hash, link, size, notice).await?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.inner
            .store
            .blobs()
            .export(hash, dest)
            .await
            .with_context(|| format!("导出到 {} 失败", dest.display()))?;

        notice(Notice::Bytes {
            done: size,
            total: Some(size),
        });
        Ok(Fetched {
            path: dest.to_path_buf(),
            bytes: size,
        })
    }

    /// 取一个文件的字节。
    ///
    /// 试过把一次下载切成 N 段、开 N 条独立连接并发取（`CHERRY_STREAMS`，已删）：**没用**。
    /// N=1/2 都是 56 MB/s，N=4 降到 51，N=8 降到 43；两个客户端同时拉也一样（各剩一半，
    /// 换成两个不同的 blob 也一样）。说明瓶颈是**每进程一个、所有连接共享的**东西，
    /// 而不是某条连接的拥塞窗口。见 docs/performance.md。
    async fn download_blob(
        &self,
        hash: Hash,
        link: &BasketLink,
        total: u64,
        notice: &mut (dyn FnMut(Notice) + Send),
    ) -> Result<()> {
        // 关键：必须用 link 里那个带完整地址的 `EndpointAddr` 建连。
        // 只给 endpoint id 的话，iroh-blobs 得自己去做地址发现，而我们把 relay
        // 关了、mDNS 又未必找得到——于是就只能等超时。
        let conn = self
            .inner
            .endpoint
            .connect(link.addr.clone(), iroh_blobs::ALPN)
            .await
            .with_context(|| format!("连不上创建者的 blobs 服务 {}", link.addr.id))?;

        let mut stream = self.inner.store.remote().fetch(conn, hash).stream();
        // 每 2% 报一次：既不淹没界面，也不让人以为卡死了。
        let step = (total / 50).max(1);
        let mut next_report = 0u64;

        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(done) => {
                    if done >= next_report {
                        notice(Notice::Bytes {
                            done,
                            total: Some(total),
                        });
                        next_report = done + step;
                    }
                }
                GetProgressItem::Done(_) => break,
                GetProgressItem::Error(e) => {
                    bail!("从创建者取 {hash} 失败: {e}")
                }
            }
        }
        Ok(())
    }

    async fn connect(&self, link: &BasketLink) -> Result<iroh::endpoint::Connection> {
        self.inner
            .endpoint
            .connect(link.addr.clone(), ALPN)
            .await
            .with_context(|| format!("连不上创建者 {}", link.addr.id))
    }

    /// 把一个文件算成 hash 并放进 store，这样查看者能按 hash 取走它。
    ///
    /// **先查索引缓存（ADR-0002、ADR-0006 决策六）。** 没有它，同一个 4GB 文件取两次
    /// 就 hash 两遍，而 mount 会让这个放大——`Open(path) → hash` 必须有便宜的来源。
    ///
    /// 注意缓存命中只解决「知道 hash」，还要核一次「字节在不在 store 里」：两件事
    /// 同时成立才发得出去（`Store::has` 只对完整 blob 返回 true）。正常路径上它们
    /// 一起成立，所以命中一次就是省下一次全量读。
    ///
    /// 还有一条已知的浪费：`add_path` 会**把文件复制进 store**——ADR-0001 里
    /// 「用 TryReference 避免创建者侧复制」那条假设还没实现。实测 200MB 的文件会让
    /// store 多出 200MB（Pi 上 store 1014MB / 源文件 960MB）。迟早得改用
    /// `ImportMode::TryReference`。
    pub async fn hash_file(&self, path: &Path) -> Result<(Hash, u64)> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat {} 失败", path.display()))?;
        let size = meta.len();
        let mtime = crate::listing::mtime_of(&meta);

        if let Some(hash) = self.inner.index.read().await.lookup(path, size, mtime)
            && self.inner.store.blobs().has(hash).await.unwrap_or(false)
        {
            return Ok((hash, size));
        }

        let tag = self
            .inner
            .store
            .blobs()
            .add_path(path)
            .await
            .with_context(|| format!("导入 {} 失败", path.display()))?;
        let hash = tag.hash;
        if let Err(e) = self.inner.index.write().await.remember(
            &self.inner.data_dir,
            path,
            size,
            mtime,
            hash,
        ) {
            // 缓存落不了盘不该让取文件失败。
            tracing::warn!(error = %e, "索引缓存没写下去");
        }
        Ok((hash, size))
    }

    // ---- 持久化 ----

    fn baskets_path(&self) -> PathBuf {
        self.inner.data_dir.join("baskets.json")
    }

    async fn persist(&self) -> Result<()> {
        let baskets = self.inner.baskets.read().await;
        let json = serde_json::to_string_pretty(&*baskets)?;
        let path = self.baskets_path();
        // 写临时文件再 rename：断电也不会留下半个配置。
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// 数据目录：服务的全部持久状态都在这儿（ADR-0003）。
pub fn default_data_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("CHERRYSHARE_DATA_DIR")
        && !d.is_empty()
    {
        return Ok(PathBuf::from(d));
    }
    let base = dirs::data_dir().context("找不到系统数据目录")?;
    Ok(base.join("cherryshare"))
}

fn load_baskets(data_dir: &Path) -> Result<Vec<Basket>> {
    let path = data_dir.join("baskets.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let s = std::fs::read_to_string(&path)
        .with_context(|| format!("读不到 {}", path.display()))?;
    serde_json::from_str(&s).with_context(|| format!("{} 解析失败", path.display()))
}

/// identity key 是所有已发出 BasketLink 的锚点：丢了它就全部失效。
/// 见 ADR-0003 的未决问题。
fn load_or_create_identity(data_dir: &Path) -> Result<iroh::SecretKey> {
    let path = data_dir.join("identity.key");
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        let bytes: [u8; 32] = hex::decode(s.trim())
            .context("identity.key 不是合法 hex")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity.key 长度不对"))?;
        return Ok(iroh::SecretKey::from_bytes(&bytes));
    }

    let secret = iroh::SecretKey::generate();
    std::fs::write(&path, hex::encode(secret.to_bytes()))?;
    restrict_permissions(&path)?;
    Ok(secret)
}

fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
