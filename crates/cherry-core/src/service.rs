//! 服务：所有语义判断的唯一落点。见 ADR-0003。
//!
//! 客户端只收集输入、呈现输出。合法性、路径规范化、冲突、状态机在这里做一次。
//!
//! **服务的全部持久状态就是 basket 配置。** 没有 identity key、没有 blobs store、
//! 没有索引缓存——ADR-0007 决策六把它们一起删了。字节也不经过这里：创建者的服务把
//! basket 讲成只读 NFS（见 `cherryd` 的 `nfs`），对方的**内核**直接读这台机器的磁盘。

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use tokio::sync::RwLock;

use crate::{basket::Basket, link::BasketLink};

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    /// 唯一的持久状态里，这份是「设计选择」而不是「OS 强制」（ADR-0003）。
    baskets: RwLock<Vec<Basket>>,
    /// NFS 导出那个监听起来了没有。`cherryd` 绑好（或绑失败）之后告诉服务。
    nfs_up: AtomicBool,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("data_dir", &self.inner.data_dir)
            .field("baskets", &self.inner.baskets)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// 起服务：建数据目录、读 basket 配置。
    pub async fn start(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("建不出数据目录 {}", data_dir.display()))?;
        let baskets = load_baskets(&data_dir)?;
        tracing::info!(baskets = baskets.len(), "cherryd 起来了");
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                baskets: RwLock::new(baskets),
                nfs_up: AtomicBool::new(false),
            }),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// NFS 导出那个监听起来了没有。
    ///
    /// 让 `cherry status` 不会在监听没起来的时候报「在跑」——那是静默失败的一种。
    pub fn set_nfs_up(&self, up: bool) {
        self.inner.nfs_up.store(up, Ordering::Relaxed);
    }

    pub fn nfs_up(&self) -> bool {
        self.inner.nfs_up.load(Ordering::Relaxed)
    }

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
            // 不泄漏「这个 id 存在但你没权限」这种区别（ADR-0007 决策二）。
            .context("这个 basket 不在我这儿")
    }

    /// 本机这个 basket 的链接。`host` 不给就自己挑一个（见 [`crate::link::lan_addrs`]）。
    ///
    /// "填哪个地址"只有这里判一次，不让每个客户端各自判一遍（ADR-0003）。
    pub async fn link(&self, basket: &str, host: Option<IpAddr>) -> Result<BasketLink> {
        let b = self.basket(basket).await?;
        let host = match host {
            Some(h) => h,
            None => crate::link::lan_addrs()?.preferred,
        };
        Ok(BasketLink::new(host, b.id))
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
