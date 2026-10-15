//! 索引缓存：`路径 → (size, mtime, hash)`。
//!
//! ADR-0002 要求的，原话：「持久化的索引缓存是必需的：没有它，比对这一步就退化成
//! 『读一遍所有内容』」。没有它，同一个 4GB 文件取两次就 hash 两遍。
//!
//! **它只解决「知道 hash」，不解决「字节在不在 store 里」。** 一个文件能被查看者下载
//! 需要两件事同时成立，所以命中的 hash 还要拿 `Store::has` 核一次——见
//! [`crate::service::Service::hash_file`]。那两件事在正常路径上是一起成立的
//! （我们不开 GC），所以命中一次就等于省下一次全量读。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use iroh_blobs::Hash;

/// 一条记录。`(size, mtime)` 就是失效判据：对不上就当它不存在。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    size: u64,
    /// Unix 秒，和 listing 里的 mtime 同口径。
    mtime: u64,
    /// hex。存成 hex 是为了 `index.json` 还能用眼睛看。
    hash: String,
}

/// 服务侧的索引缓存。持久化在数据目录里（ADR-0003：服务独占这份状态）。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    #[serde(default)]
    entries: HashMap<PathBuf, Entry>,
}

impl Index {
    /// 读缓存。文件不在就当空的——**缓存读不出来不该让服务起不来**。
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::file(data_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("读不到 {}", path.display()))?;
        serde_json::from_str(&s).with_context(|| format!("{} 解析失败", path.display()))
    }

    fn file(data_dir: &Path) -> PathBuf {
        data_dir.join("index.json")
    }

    /// 只在 `(size, mtime)` 都对得上时才认。
    ///
    /// 对不上就是「没有」，**但不顺手删掉**：删了要写盘，而下次真算出来会覆盖它。
    /// 失效 = 丢条目、重算 = 请求时（ADR-0006 决策六）。
    pub fn lookup(&self, path: &Path, size: u64, mtime: u64) -> Option<Hash> {
        let e = self.entries.get(path)?;
        (e.size == size && e.mtime == mtime)
            .then(|| e.hash.parse().ok())
            .flatten()
    }

    /// 记下来并落盘。
    pub fn remember(
        &mut self,
        data_dir: &Path,
        path: &Path,
        size: u64,
        mtime: u64,
        hash: Hash,
    ) -> Result<()> {
        self.entries.insert(
            path.to_path_buf(),
            Entry {
                size,
                mtime,
                hash: hash.to_string(),
            },
        );
        self.persist(data_dir)
    }

    /// 临时文件 + rename：断电不会留下半个文件。和 basket 配置一个路子。
    fn persist(&self, data_dir: &Path) -> Result<()> {
        let path = Self::file(data_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Hash {
        Hash::new(b"x")
    }

    #[test]
    fn 命中要求_size_和_mtime_都对得上() {
        let mut idx = Index::default();
        idx.entries.insert(
            PathBuf::from("/a"),
            Entry {
                size: 10,
                mtime: 100,
                hash: h().to_string(),
            },
        );
        let p = Path::new("/a");
        assert_eq!(idx.lookup(p, 10, 100), Some(h()));
        assert_eq!(idx.lookup(p, 11, 100), None, "size 变了要落空");
        assert_eq!(idx.lookup(p, 10, 101), None, "mtime 变了要落空");
        assert_eq!(idx.lookup(Path::new("/b"), 10, 100), None, "没记过要落空");
    }
}
