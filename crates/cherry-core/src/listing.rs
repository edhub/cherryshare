//! 目录是现算的响应，不是制品。见 ADR-0002。
//!
//! 单位是**一个目录**，不是整棵树（ADR-0002）：NFS 的 `readdir` 本来就是按
//! 目录的，而整棵树意味着「为了显示一个目录，要取回整棵树」。
//!
//! `list_dir` 只做廉价的那一半：遍历 + stat，不读内容。所以打开一个目录总是很快，
//! 哪怕创建者刚改了几十 GB。
//!
//! **共享的只有真文件和真目录。** 符号链接不共享：一个指向别处的链接等于绕开了
//! `..` 的封锁，而两者都是「爬出共享树」。判断只有一处（[`servable`]），列目录和
//! 问单个条目两条路都必须问它——不然同一件事会有两种说法，客户端拿到自相矛盾的属性。

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::basket::{Basket, CherryKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 这个目录里的**叶子名**。完整路径由请求它的那个目录拼出来：`dir + "/" + name`。
    pub name: String,
    pub kind: CherryKind,
    /// 目录记 0。
    pub size: u64,
    /// Unix 秒。目录也填真实 mtime（见 [`entry`]）。
    pub mtime: u64,
    /// 源文件的权限位（低 12 位，含执行位和那几个特殊位）。
    ///
    /// **照实报**：这是导出给对方的描述，不是我们自己的策略。写不写得了由服务端拒
    /// （NFS3ERR_ROFS）和挂载时的 `ro` 决定，跟这个数字无关；而这个数字会被对方机器上的
    /// `cp`、`rsync -a`、`tar`、Finder 拖拽拿去当**目标文件**的权限——报成 `0o444` 的话，
    /// 从共享里拷出来的一切都是只读的（连带着目录是 `0o555`，往里面加文件都加不了）。
    pub mode: u32,
}

/// 列**一个**目录。`dir` 是篮子内的相对路径，空串表示篮子根（也就是 cherry 列表）。
///
/// 篮子根这一层是唯一会把「引用失效」暴露成错误的地方：每颗 cherry 都要 `metadata`。
/// 做法是**跳过**，不是让整层失败——一条死引用不该让整个篮子打不开。而摘掉那颗 cherry
/// 是服务的事（[`crate::service::Service::prune_dead`]，ADR-0001），这里只管列。
///
/// **子目录不受影响**——这是按目录列带来的额外好处。
pub fn list_dir(basket: &Basket, dir: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    if dir.is_empty() {
        // 篮子根：cherry 本身。
        for cherry in &basket.cherries {
            match entry(&cherry.name, &cherry.source) {
                Ok(Some(e)) => out.push(e),
                // 不会是 `None`：`Basket::add` 已经把源规范化过、也拦过一道。真出现就
                // 说明 `baskets.json` 被人手改了。
                Ok(None) => tracing::warn!(cherry = %cherry.name, "不是普通文件也不是真目录，跳过"),
                // 源没了。服务发现之后会把这颗摘掉（ADR-0001）；这里是刚删掉、还没来得及
                // 摘的那一瞬。跳过就行——一条死引用不该把整层拖垮。
                Err(e) => tracing::warn!(cherry = %cherry.name, "源的元数据读不到，跳过: {e:#}"),
            }
        }
    } else {
        let (full, meta) = resolve(basket, dir)?;
        if !meta.is_dir() {
            bail!("`{dir}` 不是一个目录");
        }
        let rd = std::fs::read_dir(&full).with_context(|| format!("读不到 {}", full.display()))?;
        for de in rd {
            let de = de?;
            let name = match de.file_name().into_string() {
                Ok(n) => n,
                // 非 UTF-8 名字没法放进字符串路径，跳过但要说一声。
                Err(_) => {
                    tracing::warn!(path = %de.path().display(), "名字不是 UTF-8，跳过");
                    continue;
                }
            };
            match entry(&name, &de.path()) {
                Ok(Some(e)) => out.push(e),
                Ok(None) => tracing::warn!(path = %de.path().display(), "符号链接之类，不共享"),
                // 列目录和读元数据之间文件被删了。跳过它，别让一个竞态把整层弄失败。
                Err(e) => tracing::warn!(path = %de.path().display(), "读不到元数据，跳过: {e:#}"),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 这个元数据指向的东西能不能对外。
///
/// **共享的只有真文件和真目录。** 符号链接不共享：一个指向共享树外面的链接等于绕开了
/// `..` 的封锁。设备和 fifo 之类也不共享——共享出去的是文件，不是那台机器的延伸。
///
/// 注意要用 `symlink_metadata` 拿元数据，`metadata` 会跟随链接、看不出它是链接。
pub fn servable(meta: &std::fs::Metadata) -> Option<CherryKind> {
    if meta.is_symlink() {
        return None;
    }
    if meta.is_dir() {
        Some(CherryKind::Dir)
    } else if meta.is_file() {
        Some(CherryKind::File)
    } else {
        None
    }
}

/// 把篮子内的相对路径解析回创建者磁盘上的绝对路径。
///
/// 这是安全边界：`..`、绝对路径、以及**任何一段是符号链接**都不允许，否则查看者
/// 能读到 cherry 之外的东西。
///
/// 顺手把元数据带回来，并且在这里就把「能不能对外」判掉：调用方要判存在、判类型、
/// 要 size/mtime，全由这一次 stat 给出，别再各自 stat 一遍然后说法不一。
pub fn resolve(basket: &Basket, path: &str) -> Result<(PathBuf, std::fs::Metadata)> {
    let mut it = path.split('/');
    let root = it.next().unwrap_or_default();
    // 空串（含前导 `/`）、`.`、`..` 都先拦下：它们本来也找不到 cherry，
    // 但报「不在这个 basket 里」会让人以为只是名字打错了。
    if root.is_empty() || root == "." || root == ".." {
        bail!("路径里的 `{root}` 不合法");
    }
    let cherry = basket
        .cherry(root)
        .with_context(|| format!("`{root}` 不在这个 basket 里"))?;

    let mut full = cherry.source.clone();
    for seg in it {
        // 只在 core 里判一次，客户端不重复这个判断（ADR-0003）。
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("路径里的 `{seg}` 不合法");
        }
        if PathBuf::from(seg)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("路径里的 `{seg}` 不合法");
        }
        full.push(seg);
    }

    // 段里没有 `..` 不等于爬不出去：中间任何一段都可以是符号链接。
    //
    // 判法是「把解析后的真实路径的尾段，和请求的尾段逐段比」。只要中间跟过任何
    // 一个符号链接，两者就不一样，直接拒——这也正是「不共享符号链接」这条规则本身。
    // 只 canonicalize 一次（cherry 的源在 `Basket::add` 里已经规范化过）。
    //
    // macOS 上 `realpath` 保留传入的大小写，所以同一段名字换大小写不会被误拒。
    let real = full
        .canonicalize()
        .with_context(|| format!("读不到 {}", full.display()))?;
    let asked = full.strip_prefix(&cherry.source).unwrap_or(Path::new(""));
    let got = real
        .strip_prefix(&cherry.source)
        .with_context(|| format!("`{path}` 指到了共享范围外面"))?;
    if got != asked {
        bail!("`{path}` 中间有符号链接，不共享");
    }

    let meta = std::fs::symlink_metadata(&full)
        .with_context(|| format!("stat {} 失败", full.display()))?;
    servable(&meta).with_context(|| format!("`{path}` 不是普通文件也不是真目录，不共享"))?;
    Ok((full, meta))
}

/// `None` 表示这个条目不共享（符号链接、设备之类），调用方跳过它并说一声。
fn entry(name: &str, full: &std::path::Path) -> Result<Option<Entry>> {
    // 必须用 `symlink_metadata`：`metadata` 会跟随链接，就看不出它是链接了。
    let meta =
        std::fs::symlink_metadata(full).with_context(|| format!("stat {} 失败", full.display()))?;
    let Some(kind) = servable(&meta) else {
        return Ok(None);
    };
    let is_dir = matches!(kind, CherryKind::Dir);
    Ok(Some(Entry {
        name: name.to_owned(),
        kind,
        size: if is_dir { 0 } else { meta.len() },
        // 目录也有 mtime，而且**必须填真的**：NFS 客户端靠它判断目录列表要不要重取，
        // 填 0 就等于「永远不刷新」。
        mtime: mtime_of(&meta),
        mode: mode_of(&meta),
    }))
}

/// 源文件的权限位。判断只在这一处，列目录和问单个条目都说同一句话。
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

pub fn mtime_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试拿自己的目录：用 `process::id()` 的话几个测试会并行地互相删掉对方的目录。
    fn tree(tag: &str) -> (Basket, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cherry-listing-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::write(dir.join("ok.txt"), b"x").unwrap();
        std::fs::write(dir.join("sub/two.bin"), b"yy").unwrap();
        std::fs::write(dir.join("sub/deep/three.bin"), b"zzz").unwrap();

        let mut b = Basket::new("t").unwrap();
        b.add(&dir).unwrap();
        (b, dir)
    }

    #[test]
    fn 拒绝越界路径() {
        let (b, _d) = tree("resolve");
        let root = b.cherries[0].name.clone();
        assert!(resolve(&b, &root).is_ok());
        assert!(resolve(&b, &format!("{root}/sub")).is_ok());
        assert!(resolve(&b, "../etc/passwd").is_err());
        assert!(resolve(&b, &format!("{root}/../../etc/passwd")).is_err());
        assert!(resolve(&b, "/etc/passwd").is_err());
        assert!(resolve(&b, "nope.txt").is_err());
    }

    #[test]
    fn 列根只有名字没有整棵树() {
        let (b, _d) = tree("root");
        let root = b.cherries[0].name.clone();
        let top = list_dir(&b, "").unwrap();
        assert_eq!(top.len(), 1, "根这一层只该有 cherry 本身");
        assert_eq!(top[0].name, root);
        assert_eq!(top[0].kind, CherryKind::Dir);
    }

    #[test]
    fn 列一个目录不递归() {
        let (b, _d) = tree("onelevel");
        let root = b.cherries[0].name.clone();
        let names: Vec<_> = list_dir(&b, &root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["ok.txt", "sub"], "只列这一层，不进 sub");
    }

    #[test]
    fn 列不存在的目录要有明确错误() {
        let (b, _d) = tree("missing");
        let root = b.cherries[0].name.clone();
        assert!(list_dir(&b, &format!("{root}/nope")).is_err());
    }

    #[test]
    fn 对文件列目录要报错而不是当空目录() {
        let (b, _d) = tree("notdir");
        let root = b.cherries[0].name.clone();
        assert!(list_dir(&b, &format!("{root}/ok.txt")).is_err());
    }

    /// 跟随链接的 `metadata` 看不出它是链接——这就是必须用 `symlink_metadata` 的原因。
    #[cfg(unix)]
    #[test]
    fn 只有真文件和真目录能对外() {
        let d = std::env::temp_dir().join(format!("cherry-servable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("f");
        std::fs::write(&f, b"x").unwrap();
        let l = d.join("l");
        std::os::unix::fs::symlink(&f, &l).unwrap();

        let meta = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap();
        assert_eq!(servable(&meta(&d)), Some(CherryKind::Dir));
        assert_eq!(servable(&meta(&f)), Some(CherryKind::File));
        assert_eq!(servable(&meta(&l)), None, "符号链接不共享");
        assert_eq!(
            servable(&std::fs::metadata(&l).unwrap()),
            Some(CherryKind::File),
            "跟随链接的话就看不出它是链接了"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 源没了：跳过它就行，别让一条死引用把整个篮子根变成错误。摘掉那颗是服务的事。
    #[test]
    fn 根这一层跳过源没了的() {
        let (mut b, dir) = tree("deadroot");
        let dead = dir.join("dead.txt");
        std::fs::write(&dead, b"x").unwrap();
        b.add(&dead).unwrap();
        assert_eq!(list_dir(&b, "").unwrap().len(), 2);

        std::fs::remove_file(&dead).unwrap();
        let top = list_dir(&b, "").unwrap();
        assert_eq!(top.len(), 1, "死引用该被跳过，不是让整层失败");
        assert_eq!(top[0].name, b.cherries[0].name);
    }

    /// 符号链接不共享，也不许从它中间穿过：指到外面等于绕开 `..` 的封锁，指到里面
    /// 也不是一个真文件。两者都得拒。
    #[cfg(unix)]
    #[test]
    fn 符号链接既不共享也不许穿过() {
        let (b, dir) = tree("symlink");
        let root = b.cherries[0].name.clone();
        let outside = std::env::temp_dir().join(format!("cherry-outside-{}", std::process::id()));
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        std::os::unix::fs::symlink(dir.join("ok.txt"), dir.join("inside")).unwrap();

        // 指到外面的：规范化之后不在 cherry 里面了。
        let e = resolve(&b, &format!("{root}/escape")).unwrap_err();
        assert!(
            e.to_string().contains("共享范围外面"),
            "报错要说清爬出去了: {e}"
        );
        // 指到里面的：落在范围内，但请求的那段和磁盘上那段不是一个名字。
        let e = resolve(&b, &format!("{root}/inside")).unwrap_err();
        assert!(e.to_string().contains("符号链接"), "报错要说清是链接: {e}");

        let names: Vec<_> = list_dir(&b, &root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            !names.contains(&"escape".to_owned()),
            "列目录不该出现: {names:?}"
        );
        assert!(
            !names.contains(&"inside".to_owned()),
            "列目录不该出现: {names:?}"
        );
        let _ = std::fs::remove_file(&outside);
    }
}
