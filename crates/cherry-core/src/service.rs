//! 服务：所有语义判断的唯一落点。见 ADR-0003。
//!
//! 客户端只收集输入、呈现输出。合法性、路径规范化、冲突、状态机在这里做一次。
//!
//! **持久状态就是数据目录里那几个 `.basket` 文件**（ADR-0008）。服务读它们、盯着它们的
//! mtime，文件一变就重读；除了补一个缺失的 `id` 行，不往里面写任何东西。字节也不经过
//! 这里：创建者的服务把 basket 讲成只读 NFS（见 `cherryd` 的 `nfs`），对方的**内核**直接
//! 读这台机器的磁盘。

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, RwLock};

use crate::{
    basket::{self, Basket},
    link::BasketLink,
};

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    state: RwLock<State>,
    /// 重读这件事一次只许一个人做：两个请求同时发现「文件没 id」会各发一个 id，
    /// 内存里留一个、文件里写一个，链接就对不上了。
    refresh: Mutex<()>,
    /// NFS 导出那个监听起来了没有。`cherryd` 绑好（或绑失败）之后告诉服务。
    nfs_up: AtomicBool,
    http_up: AtomicBool,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("data_dir", &self.inner.data_dir)
            .finish_non_exhaustive()
    }
}

/// 数据目录现在的内容：读得进来的、读不进来的，以及读的时候那些文件长什么样。
pub struct Baskets {
    /// 能用的篮子，按文件名排。
    pub ok: Vec<Basket>,
    /// 读不进来的文件。它们没有 id，所以别人的链接本来就指不到，只在 CLI 里露面。
    pub broken: Vec<Broken>,
}

#[derive(Debug, Clone)]
pub struct Broken {
    /// 文件名去掉 `.basket`。
    pub name: String,
    pub file: PathBuf,
    pub error: String,
}

struct State {
    baskets: Baskets,
    /// 目录和每个篮子文件当时的 (mtime, 大小)。只要一个都没变就不重读。
    dir: Option<Stamp>,
    files: Vec<(PathBuf, Option<Stamp>)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// 纳秒。APFS 上够细，改一下就能看出来。
    mtime: i128,
    size: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    Some(Stamp {
        mtime,
        size: meta.len(),
    })
}

impl Service {
    /// 起服务：建数据目录、把老格式迁过来、读一遍篮子。
    pub async fn start(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("建不出数据目录 {}", data_dir.display()))?;
        migrate(&data_dir);
        let state = load_all(&data_dir, &[]);
        tracing::info!(
            baskets = state.baskets.ok.len(),
            broken = state.baskets.broken.len(),
            "cherryd 起来了"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                state: RwLock::new(state),
                refresh: Mutex::new(()),
                nfs_up: AtomicBool::new(false),
                http_up: AtomicBool::new(false),
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

    pub fn set_http_up(&self, up: bool) {
        self.inner.http_up.store(up, Ordering::Relaxed);
    }

    /// 网页那个监听起没起来。
    pub fn http_up(&self) -> bool {
        self.inner.http_up.load(Ordering::Relaxed)
    }

    /// 现在数据目录里有什么。**每个请求都先过这儿**，文件改了就在这一刻跟上。
    pub async fn baskets(&self) -> Baskets {
        self.refresh().await;
        let state = self.inner.state.read().await;
        Baskets {
            ok: state.baskets.ok.clone(),
            broken: state.baskets.broken.clone(),
        }
    }

    /// 按篮子名取一个（就是文件名）。读不进来的文件把原因原样报出来。
    pub async fn basket(&self, name: &str) -> Result<Basket> {
        let all = self.baskets().await;
        if let Some(b) = all.ok.into_iter().find(|b| b.name == name) {
            return Ok(b);
        }
        if let Some(b) = all.broken.iter().find(|b| b.name == name) {
            bail!("{} 读不进来：{}", b.file.display(), b.error);
        }
        bail!(
            "没有叫 `{name}` 的 basket（{} 里写个 {name}.basket 就有）",
            self.inner.data_dir.display()
        )
    }

    pub async fn basket_by_id(&self, id: &[u8; 16]) -> Result<Basket> {
        self.baskets()
            .await
            .ok
            .into_iter()
            .find(|b| &b.id == id)
            // 不泄漏「这个 id 存在但你没权限」这种区别（ADR-0007 决策二）。
            .context("这个 basket 不在我这儿")
    }

    /// 这个 basket 能发出去的几条链接，顺序就是 [`crate::link::candidate_hosts`] 给的
    /// 顺序：本机名在前，IP 在后。
    ///
    /// 两条都给，因为发送方判不出对方在不在同一个广播域，挑哪条是对方那边的事
    /// （ADR-0002）。只有一条都算不出来的机器上才报错。
    pub async fn links(&self, basket: &str) -> Result<Vec<BasketLink>> {
        self.links_of(&self.basket(basket).await?)
    }

    /// 已经拿到篮子时直接算链接，不用再按名字查一遍。
    pub fn links_of(&self, b: &Basket) -> Result<Vec<BasketLink>> {
        let hosts = crate::link::candidate_hosts();
        if hosts.is_empty() {
            bail!("这台机器上既没有本机名，也没有可用的局域网地址");
        }
        Ok(hosts
            .into_iter()
            .map(|host| BasketLink::new(host, b.id))
            .collect())
    }

    /// 默认那一条：本机名优先，拿不到就退到 IP。网页那边没拿到 Host 头时用它。
    pub async fn link(&self, basket: &str) -> Result<BasketLink> {
        self.link_of(&self.basket(basket).await?)
    }

    pub fn link_of(&self, b: &Basket) -> Result<BasketLink> {
        let host = crate::link::candidate_hosts()
            .into_iter()
            .next()
            .context("这台机器上既没有本机名，也没有可用的局域网地址")?;
        Ok(BasketLink::new(host, b.id))
    }

    /// 文件变了就重读。
    ///
    /// 不用 inotify：几个 stat 比请求本身便宜，而且没有漏事件、FSEvents 合并这类边界要
    /// 操心。「没人来问的时候热加载」本来也没有意义。
    async fn refresh(&self) {
        let _one_at_a_time = self.inner.refresh.lock().await;
        let dir = stamp_of(&self.inner.data_dir);
        {
            let state = self.inner.state.read().await;
            if state.dir == dir && state.files.iter().all(|(p, s)| stamp_of(p) == *s) {
                return;
            }
        }
        let previous = {
            let state = self.inner.state.read().await;
            state.baskets.ok.clone()
        };
        let fresh = load_all(&self.inner.data_dir, &previous);
        *self.inner.state.write().await = fresh;
    }
}

/// 数据目录：服务的全部持久状态都在这儿（ADR-0003）。
///
/// `~/.config/cherryshare`，跟着 `XDG_CONFIG_HOME`。这不是**状态**而是你要手改的配置
/// （ADR-0008），所以放配置目录、不跟着 `dirs::config_dir()` 走——macOS 上那是
/// `~/Library/Application Support`，看不见、改起来难受。
pub fn default_data_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("CHERRYSHARE_DATA_DIR")
        && !d.is_empty()
    {
        return Ok(PathBuf::from(d));
    }
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => dirs::home_dir().context("找不到家目录")?.join(".config"),
    };
    Ok(base.join("cherryshare"))
}

/// 网页文件放在哪儿。
///
/// 跟数据目录分开：那边是你写的篮子，这边是随包一起装的静态文件，换页面不该碰到篮子。
/// 位置跟着二进制走（`~/.local/`，跟 install.sh 装二进制的地方同源），
/// 所以 `CHERRYSHARE_WEB_DIR` 只是开发时指到仓库里构建产物的旋钮，不是给用户用的。
pub fn default_web_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("CHERRYSHARE_WEB_DIR")
        && !d.is_empty()
    {
        return Ok(PathBuf::from(d));
    }
    let home = dirs::home_dir().context("找不到家目录")?;
    Ok(home.join(".local/share/cherryshare/web"))
}

/// 读整个数据目录。一个文件读坏了只影响它自己：记一条日志、进错误表，别的照常。
fn load_all(dir: &Path, previous: &[Basket]) -> State {
    let mut state = State {
        baskets: Baskets {
            ok: Vec::new(),
            broken: Vec::new(),
        },
        dir: stamp_of(dir),
        files: Vec::new(),
    };

    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_basket_file(p))
            .collect(),
        Err(e) => {
            state.baskets.broken.push(Broken {
                name: dir.display().to_string(),
                file: dir.to_path_buf(),
                error: format!("读不了数据目录: {e}"),
            });
            return state;
        }
    };
    paths.sort();

    for path in paths {
        state.files.push((path.clone(), stamp_of(&path)));
        match basket::load(&path) {
            Ok(b) => {
                if let Some(old) = previous.iter().find(|o| o.file == path)
                    && old.id != b.id
                {
                    tracing::warn!(
                        basket = %b.name,
                        old = %hex::encode(old.id), new = %hex::encode(b.id),
                        "这个篮子的 id 变了（文件里的 id 行被删掉了？），发给别人的旧链接不再有效"
                    );
                }
                state.baskets.ok.push(b);
            }
            Err(e) => {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                tracing::error!(basket = %name, file = %path.display(), "篮子读不进来: {e:#}");
                state.baskets.broken.push(Broken {
                    name,
                    file: path,
                    error: format!("{e:#}"),
                });
            }
        }
    }
    state
}

/// 只认 `<名字>.basket`，且不以 `.` 开头——不然编辑器留下的 `.foo.basket.swp`、隐藏文件
/// 也会被当成篮子。
fn is_basket_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with('.') || !name.ends_with(basket::SUFFIX) {
        return false;
    }
    let stem = &name[..name.len() - basket::SUFFIX.len()];
    !stem.is_empty() && path.is_file()
}

/// ADR-0008 之前的那种 `baskets.json`：一次性拆成一个个 `.basket` 文件。
///
/// 只在数据目录里一个 `.basket` 都没有的时候做，原文件改名留档、不删。
///
/// 老的默认目录也看一眼——换了默认目录之后，篮子不能凭空消失。但**只在 `dir` 就是当前
/// 默认数据目录时才看**：测试、e2e、第二个实例都用临时目录，它们不该把真正的那份
/// `baskets.json` 搬到自己这儿来（也就不会把真正的文件改掉名）。
fn migrate(dir: &Path) {
    let has_baskets = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| is_basket_file(&e.path()))
        })
        .unwrap_or(false);
    if has_baskets {
        return;
    }

    let mut candidates = vec![dir.join("baskets.json")];
    // 「就是真安装时用的那个数据目录」：没通过环境变量指定（那是测试、e2e、第二个实例），
    // 而且 `dir` 正是默认位置。两条都得满足。
    let installed = std::env::var("CHERRYSHARE_DATA_DIR")
        .map(|d| d.is_empty())
        .unwrap_or(true)
        && default_data_dir().ok().as_deref() == Some(dir);
    if installed
        && let Some(legacy) = dirs::data_dir().map(|d| d.join("cherryshare/baskets.json"))
        && legacy.parent() != Some(dir)
    {
        candidates.push(legacy);
    }

    for json in candidates {
        let Ok(text) = std::fs::read_to_string(&json) else {
            continue;
        };
        match serde_json::from_str::<Vec<LegacyBasket>>(&text) {
            Ok(baskets) => {
                for b in &baskets {
                    let file = dir.join(format!("{}{}", b.name, basket::SUFFIX));
                    let mut body = String::from(
                        "# 从 baskets.json 迁过来的（ADR-0008）。这个文件就是篮子的全部定义。\n",
                    );
                    body.push_str(&format!("id {}\n", hex::encode(b.id)));
                    for c in &b.cherries {
                        body.push_str(&format!("{}\n", c.source.display()));
                    }
                    if let Err(e) = std::fs::write(&file, body) {
                        tracing::error!(file = %file.display(), "迁移写不进去: {e}");
                    }
                }
                let kept = json.with_extension("json.migrated");
                if let Err(e) = std::fs::rename(&json, &kept) {
                    tracing::error!(file = %json.display(), "迁移完了，但原文件改不了名: {e}");
                }
                tracing::info!(
                    baskets = baskets.len(),
                    from = %json.display(), kept = %kept.display(),
                    "把 baskets.json 拆成 .basket 文件了"
                );
            }
            Err(e) => tracing::error!(file = %json.display(), "老的 baskets.json 解析不了: {e}"),
        }
        // 迁一份就够：另一个候选位置多半不存在。
        return;
    }
}

#[derive(serde::Deserialize)]
struct LegacyBasket {
    id: [u8; 16],
    name: String,
    cherries: Vec<LegacyCherry>,
}

#[derive(serde::Deserialize)]
struct LegacyCherry {
    source: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Host;

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cherry-svc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_basket(dir: &Path, name: &str, body: &str) -> PathBuf {
        let file = dir.join(format!("{name}.basket"));
        std::fs::write(&file, body).unwrap();
        file
    }

    #[tokio::test]
    async fn 文件里写的篮子就是能用的篮子() {
        let dir = tempdir("load");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"x").unwrap();
        write_basket(&dir, "t", &format!("# 说明\n{}/src\n", dir.display()));

        let svc = Service::start(dir.clone()).await.unwrap();
        let b = svc.basket("t").await.unwrap();
        assert_eq!(b.name, "t");
        assert_eq!(b.cherries.len(), 1);
        assert_eq!(b.cherries[0].name, "src");
        assert!(!svc.links("t").await.unwrap().is_empty());

        // 换一个 Service 从头读：id 得一样（它落在文件里），篮子还在。
        let again = Service::start(dir.clone()).await.unwrap();
        assert_eq!(again.basket("t").await.unwrap().id, b.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件是唯一真相：改了文件，不用重起就跟着变；删掉文件，篮子就没了。
    #[tokio::test]
    async fn 文件改了马上跟上() {
        let dir = tempdir("hot");
        let a = dir.join("a");
        let b = dir.join("b");
        for p in [&a, &b] {
            std::fs::create_dir_all(p).unwrap();
        }
        let file = write_basket(&dir, "t", &format!("{}\n", a.display()));
        let svc = Service::start(dir.clone()).await.unwrap();
        assert_eq!(svc.basket("t").await.unwrap().cherries[0].name, "a");

        std::fs::write(&file, format!("{}\n{}\n", a.display(), b.display())).unwrap();
        assert_eq!(svc.basket("t").await.unwrap().cherries.len(), 2);

        std::fs::remove_file(&file).unwrap();
        assert!(svc.basket("t").await.is_err(), "文件删了篮子就没了");
        assert!(svc.baskets().await.ok.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个文件读坏了只影响它自己，原因原样报给 `cherry show`。
    #[tokio::test]
    async fn 坏文件进错误表_别的照常() {
        let dir = tempdir("broken");
        write_basket(&dir, "好的", "~/Pictures\n");
        write_basket(&dir, "坏的", "./相对路径\n");
        std::fs::write(dir.join("notes.txt"), "不是篮子").unwrap();
        std::fs::write(dir.join(".隐藏.basket"), "~/x\n").unwrap();

        let svc = Service::start(dir.clone()).await.unwrap();
        let all = svc.baskets().await;
        assert_eq!(all.ok.len(), 1);
        assert_eq!(all.ok[0].name, "好的");
        assert_eq!(all.broken.len(), 1);
        assert_eq!(all.broken[0].name, "坏的");
        assert!(
            all.broken[0].error.contains("第 1 行"),
            "{}",
            all.broken[0].error
        );

        let err = svc.basket("坏的").await.unwrap_err().to_string();
        assert!(err.contains("读不进来"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源没了不再写回文件（ADR-0008）：文件是你的，工具只读。列目录照样跳过它。
    #[tokio::test]
    async fn 源没了不写回文件() {
        let dir = tempdir("gone");
        let gone = dir.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let file = write_basket(&dir, "t", &format!("{}\n", gone.display()));
        let svc = Service::start(dir.clone()).await.unwrap();
        svc.basket("t").await.unwrap();

        std::fs::remove_dir_all(&gone).unwrap();
        let b = svc.basket("t").await.unwrap();
        assert_eq!(b.cherries.len(), 1, "内存里也留着，只有列目录的时候跳过");
        assert!(crate::listing::list_dir(&b, "").unwrap().is_empty());

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains(&gone.display().to_string()),
            "文件不该被改：{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 老的_baskets_json_会拆成文件() {
        let dir = tempdir("migrate");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let json = serde_json::json!([{
            "id": [7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7],
            "name": "老篮子",
            "cherries": [{"name": "src", "source": src.to_string_lossy(), "kind": "Dir"}],
        }]);
        std::fs::write(dir.join("baskets.json"), json.to_string()).unwrap();

        let svc = Service::start(dir.clone()).await.unwrap();
        let b = svc.basket("老篮子").await.unwrap();
        assert_eq!(b.id, [7u8; 16]);
        assert_eq!(b.cherries.len(), 1);
        assert!(dir.join("baskets.json.migrated").exists(), "原文件留档");
        assert!(!dir.join("baskets.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 名字那条和 IP 那条都得在，而且是名字在前（ADR-0002）。
    #[tokio::test]
    async fn 链接给名字和_ip_两条() {
        let dir = tempdir("links");
        write_basket(&dir, "t", "~/Pictures\n");
        let svc = Service::start(dir.clone()).await.unwrap();

        let links = svc.links("t").await.unwrap();
        assert!(!links.is_empty(), "总得给人一条");

        // 本机没有对外地址的机器（容器之类）上就只有名字那条，那就没得比。
        let (Ok(ip), Some(name)) = (crate::link::lan_addr(), crate::link::local_hostname()) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        assert_eq!(links[0].host, name, "名字在前");
        assert_eq!(links[1].host, Host::Ip(ip), "IP 在后");
        assert_eq!(svc.link("t").await.unwrap().host, name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 数据目录跟着_xdg_config_home() {
        // 这个测试只验证拼法：环境变量全局共享，改它得小心，所以用子 shell 语义的那套不了，
        // 直接验「不带 XDG 时是 ~/.config/cherryshare」这一半。
        if std::env::var("XDG_CONFIG_HOME").is_err()
            && std::env::var("CHERRYSHARE_DATA_DIR").is_err()
        {
            let home = dirs::home_dir().unwrap();
            assert_eq!(
                default_data_dir().unwrap(),
                home.join(".config/cherryshare")
            );
        }
    }
}
