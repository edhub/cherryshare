//! 目录是现算的响应，不是制品。见 ADR-0002。
//!
//! 单位是**一个目录**，不是整棵树（ADR-0006 决策二）：FUSE 的 `readdir` 本来就是按
//! 目录的，而整棵树意味着「为了显示一个目录，要取回整棵树」。
//!
//! `list_dir` 只做廉价的那一半：遍历 + stat，不算 hash（那在 `GetFile` 时才做）。
//! 所以打开一个目录总是很快，哪怕创建者刚改了几十 GB。

use std::path::{Component, PathBuf};

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
    /// Unix 秒。目录没有 mtime，记为 0。
    pub mtime: u64,
}

/// 列**一个**目录。`dir` 是篮子内的相对路径，空串表示篮子根（也就是 cherry 列表）。
///
/// 缺口（比整棵树扫描时小多了）：`dir` 为空时每一颗 cherry 都要 `metadata`，其中任何
/// 一颗的源文件被删掉，「根」这一层就整个挂掉。listing 是「活的索引」，一个坏条目该被
/// 标记为缺失、而不是让整单失败。怎么选还没定。**子目录不受影响**——这是按目录列
/// 带来的额外好处。
pub fn list_dir(basket: &Basket, dir: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    if dir.is_empty() {
        // 篮子根：cherry 本身。
        for cherry in &basket.cherries {
            out.push(entry(&cherry.name, &cherry.source)?);
        }
    } else {
        let full = resolve(basket, dir)?;
        let meta = std::fs::metadata(&full)
            .with_context(|| format!("stat {} 失败", full.display()))?;
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
            out.push(entry(&name, &de.path())?);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 把篮子内的相对路径解析回创建者磁盘上的绝对路径。
///
/// 这是安全边界：`..` 和绝对路径都不允许，否则查看者能读到 cherry 之外的东西。
pub fn resolve(basket: &Basket, path: &str) -> Result<PathBuf> {
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
    Ok(full)
}

fn entry(name: &str, full: &std::path::Path) -> Result<Entry> {
    let meta = std::fs::metadata(full).with_context(|| format!("stat {} 失败", full.display()))?;
    Ok(Entry {
        name: name.to_owned(),
        kind: if meta.is_dir() {
            CherryKind::Dir
        } else {
            CherryKind::File
        },
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime: mtime_of(&meta),
    })
}

pub(crate) fn mtime_of(meta: &std::fs::Metadata) -> u64 {
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
}
