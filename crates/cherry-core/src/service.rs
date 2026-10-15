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

    /// 本机有哪些 basket。报出去之前先看一眼引用还指得着不（ADR-0001）。
    pub async fn list_baskets(&self) -> Vec<Basket> {
        let (out, removed) = {
            let mut baskets = self.inner.baskets.write().await;
            let removed: usize = baskets.iter_mut().map(prune_basket).sum();
            (baskets.clone(), removed)
        };
        if removed > 0 {
            self.persist_or_log().await;
        }
        out
    }

    pub async fn basket(&self, name: &str) -> Result<Basket> {
        // `show`、`link` 这些都经过这儿：报出去的该是现在的真相，不是刚加进来时的样子。
        let id = self.basket_id(name).await?;
        self.prune_dead(&id).await;
        self.basket_by_id(&id).await
    }

    async fn basket_id(&self, name: &str) -> Result<[u8; 16]> {
        self.inner
            .baskets
            .read()
            .await
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.id)
            .with_context(|| format!("没有叫 `{name}` 的 basket"))
    }

    /// 摘掉源已经没了的 cherry，返回摘掉几颗。
    ///
    /// 「失效」只认**确定不存在**：权限、IO、外置盘掉线之类可能是一时的，那种摘掉就再也
    /// 回不来了，所以留着，只记一条日志。见 ADR-0001。
    pub async fn prune_dead(&self, id: &[u8; 16]) -> usize {
        let removed = {
            let mut baskets = self.inner.baskets.write().await;
            match baskets.iter_mut().find(|b| &b.id == id) {
                Some(b) => prune_basket(b),
                None => 0,
            }
        };
        if removed > 0 {
            self.persist_or_log().await;
        }
        removed
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

    /// 篮子定义那个文件的 mtime。
    ///
    /// 这是给 NFS 用的：篮子根在磁盘上没有对应物，是个虚拟目录，但它得有一个**真的**
    /// 时间。而「篮子的定义变了吗」是有对应物的——配置文件本身就是这件事的记录：
    /// 增、删、改一颗 cherry 都会碰它，改文件内容不会（那是文件自己的 mtime 管的事）。
    ///
    /// 文件不存在就报 0（正常跑不到：能问到篮子根，说明那个篮子已经在配置里了）。
    pub fn baskets_mtime(&self) -> u64 {
        std::fs::metadata(self.baskets_path())
            .map(|m| crate::listing::mtime_of(&m))
            .unwrap_or(0)
    }

    /// 摘掉失效引用之后写盘。写不下去不能算了：内存里摘了、磁盘上还在，下次重起它又回来。
    async fn persist_or_log(&self) {
        if let Err(e) = self.persist().await {
            tracing::error!("写回 baskets.json 失败，摘掉的那几颗下次重起还会回来: {e:#}");
        }
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
    let s = std::fs::read_to_string(&path).with_context(|| format!("读不到 {}", path.display()))?;
    serde_json::from_str(&s).with_context(|| format!("{} 解析失败", path.display()))
}

/// 摘掉一个篮子里源已经没了的 cherry，返回摘掉几颗。
///
/// **「失效」只认确定不存在**（`NotFound`）。权限、IO、外置盘掉线之类的错可能是一时的，
/// 摘掉就回不来了——那种留着，只记日志，让下一次报错去说。
///
/// 摘的时候喊一声，日志里带上把同一个路径加回来的命令：这条引用存在过的记录只剩它了。
fn prune_basket(basket: &mut Basket) -> usize {
    let name = basket.name.clone();
    let before = basket.cherries.len();
    basket.cherries.retain(|c| {
        let gone = match std::fs::symlink_metadata(&c.source) {
            Ok(meta) => crate::listing::servable(&meta).is_none(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(
                    basket = %name, cherry = %c.name, path = %c.source.display(),
                    "源的元数据读不到，但这不像「没了」，先留着: {e}"
                );
                false
            }
        };
        if gone {
            tracing::warn!(
                basket = %name, cherry = %c.name, path = %c.source.display(),
                "源没了，把这颗 cherry 从篮子里摘掉。要放回来：cherry add {name} {}",
                c.source.display()
            );
        }
        !gone
    });
    before - basket.cherries.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basket::{Cherry, CherryKind};

    fn 引用(name: &str, source: &str) -> Cherry {
        Cherry {
            name: name.into(),
            source: PathBuf::from(source),
            kind: CherryKind::File,
        }
    }

    #[test]
    fn 确定没了才摘() {
        let mut b = Basket::new("t").unwrap();
        b.cherries.push(引用("x", "/no/such/thing"));
        assert_eq!(prune_basket(&mut b), 1);
        assert!(b.cherries.is_empty());
    }

    /// 读元数据时的错不都是「没了」：权限、IO 这类可能是一时的，摘掉就回不来。
    /// `/etc/hosts/x` 拿到的是 `ENOTDIR`——那是个确定的错，但不是「源没了」。
    #[test]
    fn 一时的错误不算失效() {
        let mut b = Basket::new("t").unwrap();
        b.cherries.push(引用("x", "/etc/hosts/deeper"));
        assert_eq!(prune_basket(&mut b), 0);
        assert_eq!(b.cherries.len(), 1, "不该摘掉");
    }

    /// 源没了：报出去的篮子里就不该再有它，而且磁盘上也得落下来（不然重起又回来了）。
    #[tokio::test]
    async fn 摘掉源没了的引用并落盘() {
        let data = std::env::temp_dir().join(format!("cherry-svc-{}", std::process::id()));
        let src = data.join("src");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"x").unwrap();

        let svc = Service::start(data.clone()).await.unwrap();
        svc.create_basket("t").await.unwrap();
        svc.add_cherry("t", &src).await.unwrap();
        assert_eq!(svc.basket("t").await.unwrap().cherries.len(), 1);

        std::fs::remove_dir_all(&src).unwrap();
        assert!(svc.basket("t").await.unwrap().cherries.is_empty());
        assert!(svc.list_baskets().await[0].cherries.is_empty());

        // 换一个 Service 从头读盘：摘掉这件事得是持久的。
        let again = Service::start(data.clone()).await.unwrap();
        assert!(again.basket("t").await.unwrap().cherries.is_empty());

        let _ = std::fs::remove_dir_all(&data);
    }
}
