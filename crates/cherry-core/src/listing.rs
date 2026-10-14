//! 目录是现算的响应，不是制品。见 ADR-0002。
//!
//! `scan` 只做廉价的那一半：遍历 + stat，不算 hash。所以打开一个 basket 总是
//! 很快，哪怕创建者刚改了几十 GB。

use std::path::{Component, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::basket::{Basket, CherryKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 篮子内的相对路径：`cherry 名 + 子路径`。
    pub path: String,
    pub kind: CherryKind,
    pub size: u64,
    /// Unix 秒。目录没有 mtime，记为 0。
    pub mtime: u64,
}

/// 遍历整个 basket。耗时只与 cherry 数量有关，与内容变了多少无关。
///
/// 缺口：任何一棵 cherry 的源文件被删掉，`metadata` 报错就整个 listing 挂掉——
/// 查看者会因此看不到篮子里其他好好的东西。listing 是「活的索引」，
/// 一个坏条目该被标记为缺失、而不是让整单失败。怎么选还没定。
pub fn scan(basket: &Basket) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for cherry in &basket.cherries {
        match cherry.kind {
            CherryKind::File => out.push(entry(&cherry.name, &cherry.source)?),
            CherryKind::Dir => walk(&cherry.name, &cherry.source, &mut out)?,
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 把 listing 里的一条路径解析回创建者磁盘上的绝对路径。
///
/// 这是安全边界：`..` 和绝对路径都不允许，否则查看者能读到 cherry 之外的东西。
pub fn resolve(basket: &Basket, path: &str) -> Result<PathBuf> {
    let mut it = path.split('/');
    let root = it.next().unwrap_or_default();
    let cherry = basket
        .cherry(root)
        .with_context(|| format!("`{root}` 不在这个 basket 里"))?;

    let mut full = cherry.source.clone();
    for seg in it {
        // 只在 core 里判一次，客户端不重复这个判断（ADR-0003）。
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("路径里的 `{seg}` 不合法");
        }
        if PathBuf::from(seg).components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("路径里的 `{seg}` 不合法");
        }
        full.push(seg);
    }
    Ok(full)
}

fn walk(prefix: &str, dir: &std::path::Path, out: &mut Vec<Entry>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("读不到 {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            // 非 UTF-8 名字没法放进 listing 的字符串路径里，跳过但要说一声。
            Err(_) => {
                tracing::warn!(path = %path.display(), "名字不是 UTF-8，跳过");
                continue;
            }
        };
        let child = format!("{prefix}/{name}");
        if path.is_dir() {
            walk(&child, &path, out)?;
        } else {
            out.push(entry_for(&child, &path)?);
        }
    }
    Ok(())
}

fn entry(path: &str, full: &std::path::Path) -> Result<Entry> {
    let meta = std::fs::metadata(full).with_context(|| format!("stat {} 失败", full.display()))?;
    Ok(Entry {
        path: path.to_owned(),
        kind: if meta.is_dir() {
            CherryKind::Dir
        } else {
            CherryKind::File
        },
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime: mtime_of(&meta),
    })
}

fn entry_for(path: &str, full: &std::path::Path) -> Result<Entry> {
    entry(path, full)
}

fn mtime_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拒绝越界路径() {
        let dir = std::env::temp_dir().join("cherry-listing-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.txt"), b"x").unwrap();

        let mut b = Basket::new("t").unwrap();
        b.add(&dir.join("ok.txt")).unwrap();

        assert!(resolve(&b, "ok.txt").is_ok());
        assert!(resolve(&b, "../etc/passwd").is_err());
        assert!(resolve(&b, "ok.txt/../../etc/passwd").is_err());
        assert!(resolve(&b, "/etc/passwd").is_err());
        assert!(resolve(&b, "nope.txt").is_err());
    }
}
