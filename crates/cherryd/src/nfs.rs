//! 把篮子当只读 NFS 导出去（ADR-0007）。
//!
//! 为什么讲 NFS 而不是写 FUSE：macOS 上 macFUSE 要 kext，而 FUSE-T 自己就是
//! 「在本机跑一个 NFS 服务再挂回环」。既然内核自带 NFS 客户端，直接讲 NFS 就少一层
//! 依赖，还白得内核的缓存、预读和超时重试——那些用 FUSE 得自己写。
//!
//! **权限模型是能力令牌，不是账号。** 挂载路径的第一段必须是篮子的 128 位 id，
//! 不知道 id 就挂不上（ADR-0002：`basket_id` 是凭证，不是索引）。所以 [`path_to_id`]
//! 里 `"/"` 必须直接拒：全局根一旦交出去，客户端 `readdir` 一下就能拿到所有篮子 id，
//! 「密码」就白设了。`lookup` 里的 `".."` 同理，走到篮子根就到头。
//!
//! **所有磁盘操作都走 `spawn_blocking`。** cherryd 的 tokio worker 默认只有 1 个
//! （见 [`crate::runtime`]），一个阻塞的 `stat` 会把整个守护进程卡住——包括同时
//! 在跑的 basket 管理请求。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use cherry_core::listing::{self, Entry};
use cherry_core::{Basket, CherryKind, Service};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
};
use nfsserve::tcp::NFSTcpListener;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

/// 把篮子挂起来。`bind` 到 `0.0.0.0:{port}`：对方在局域网上直接连。
///
/// 端口被占就返回 Err，由调用方决定是响亮地失败还是继续跑。
pub async fn serve(svc: Service, port: u16) -> std::io::Result<NFSTcpListener<BasketNfs>> {
    let nfs = BasketNfs {
        svc,
        ids: Mutex::new(Arena::default()),
    };
    // export_name 保持默认的 "/"：一个监听器服务所有篮子，靠路径里第一段 id 路由。
    NFSTcpListener::bind(&format!("0.0.0.0:{port}"), nfs).await
}

pub struct BasketNfs {
    svc: Service,
    ids: Mutex<Arena>,
}

/// fileid ↔（篮子 id，篮子内相对路径）的映射。
///
/// 客户端会缓存 fileid（它就是 NFS 的 inode 号），所以同一个路径必须永远拿到同一个 id。
/// 反过来，一个 id 指向的文件被删了就该报 `STALE`，让客户端丢掉句柄。
#[derive(Default)]
struct Arena {
    next: fileid3,
    by_id: HashMap<fileid3, Key>,
    by_key: HashMap<Key, fileid3>,
}

/// 篮子的 128 位 id，加上篮子内的相对路径。空串表示篮子根。
type Key = ([u8; 16], String);

impl Arena {
    fn intern(&mut self, key: Key) -> fileid3 {
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }
        // 0 是全局根，不分配出去（nfsserve 的约定）。
        self.next += 1;
        let id = self.next;
        self.by_key.insert(key.clone(), id);
        self.by_id.insert(id, key);
        id
    }

    fn key(&self, id: fileid3) -> Option<&Key> {
        self.by_id.get(&id)
    }
}

impl BasketNfs {
    /// fileid →（篮子，篮子内相对路径）。
    ///
    /// 篮子每次现问服务，不在自己这里存副本——所以创建者增删 cherry 是立刻可见的
    /// （客户端那边能多快看见，取决于它的属性缓存，见挂载时的 `actimeo`）。
    async fn target(&self, id: fileid3) -> Result<(Basket, String), nfsstat3> {
        if id == 0 {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let key = {
            let ids = self.ids.lock().unwrap();
            ids.key(id).cloned()
        }
        .ok_or(nfsstat3::NFS3ERR_STALE)?;
        // 篮子被删掉了，句柄也就没意义了。
        let basket = self
            .svc
            .basket_by_id(&key.0)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_STALE)?;
        Ok((basket, key.1))
    }

    /// 篮子内相对路径 → 磁盘路径**和它的元数据**。
    ///
    /// 篮子根返回 `None`：它是个虚拟目录（成员是 cherry 本身），磁盘上没有对应物。
    async fn disk_path(
        basket: &Basket,
        sub: &str,
    ) -> Result<Option<(PathBuf, std::fs::Metadata)>, nfsstat3> {
        if sub.is_empty() {
            return Ok(None);
        }
        let (b, s) = (basket.clone(), sub.to_owned());
        tokio::task::spawn_blocking(move || listing::resolve(&b, &s))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map(Some)
            // 「路径不存在」和「路径非法」回同一个错：别让人靠错误码探测。
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)
    }

    async fn listing(basket: &Basket, sub: &str) -> Result<Vec<Entry>, nfsstat3> {
        let (b, s) = (basket.clone(), sub.to_owned());
        tokio::task::spawn_blocking(move || listing::list_dir(&b, &s))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)
    }

    /// 只读导出，权限位怎么报。
    ///
    /// **写位一律清掉**，哪怕源文件可写：这个导出不接受写，报可写就是骗客户端。
    /// **执行位照实报**：一个二进制在挂载点上被报成 `0o444`，对方 `cp` 出来就没有
    /// 执行位、跑不起来。共享一包可执行文件是正经用例。
    fn perm(is_dir: bool, exec: bool) -> u32 {
        if is_dir || exec { 0o555 } else { 0o444 }
    }

    fn fattr(id: fileid3, is_dir: bool, exec: bool, size: u64, mtime: u64) -> fattr3 {
        let t = nfstime3 {
            seconds: mtime.min(u32::MAX as u64) as u32,
            nseconds: 0,
        };
        fattr3 {
            ftype: if is_dir {
                ftype3::NF3DIR
            } else {
                ftype3::NF3REG
            },
            mode: Self::perm(is_dir, exec),
            nlink: 1,
            // uid/gid 一律报 0，不透创建者的身份：这是共享出去的一份只读内容，
            // 不是那台机器延伸出来的文件系统。
            uid: 0,
            gid: 0,
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

}

#[async_trait]
impl NFSFileSystem for BasketNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    fn root_dir(&self) -> fileid3 {
        // 0 是全局根，只用来满足 trait 的约定，永远不对外（见 path_to_id）。
        0
    }

    /// MNT 请求里的路径。这是唯一的门禁点。
    async fn path_to_id(&self, path: &[u8]) -> Result<fileid3, nfsstat3> {
        let s = std::str::from_utf8(path).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        let rest = s.trim_matches('/');
        // 挂 "/" 直接拒。这是这个设计成立的前提，不是细节。
        if rest.is_empty() {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let (hexid, sub) = match rest.split_once('/') {
            Some((a, b)) => (a, b.trim_matches('/')),
            None => (rest, ""),
        };
        let id = parse_id(hexid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let basket = self
            .svc
            .basket_by_id(&id)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if !sub.is_empty() {
            Self::disk_path(&basket, sub).await.map(|_| ())?;
        }
        Ok(self.ids.lock().unwrap().intern((id, sub.to_owned())))
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        let (basket, sub) = self.target(dirid).await?;
        let child = match name {
            // 往上走。走到篮子根就停——绝不能返回全局根，那会把所有 id 暴露出去。
            ".." => match sub.rsplit_once('/') {
                Some((p, _)) => p.to_owned(),
                None => String::new(),
            },
            "." => sub,
            _ => {
                // 一个名字必须是一段，不能带 '/'。
                if name.is_empty() || name.contains('/') {
                    return Err(nfsstat3::NFS3ERR_NOENT);
                }
                if sub.is_empty() {
                    name.to_owned()
                } else {
                    format!("{sub}/{name}")
                }
            }
        };
        if !child.is_empty() {
            Self::disk_path(&basket, &child).await.map(|_| ())?;
        }
        Ok(self.ids.lock().unwrap().intern((basket.id, child)))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let (basket, sub) = self.target(id).await?;
        match Self::disk_path(&basket, &sub).await? {
            // 和列目录是**同一个来源、同一句判断**：size 和 mtime 都出自这一次 stat。
            // 两边说法不一致的话，客户端的缓存就自相矛盾（目录报目录、这里报文件），
            // 行为会变成看客户端心情。
            Some((_path, meta)) => {
                let is_dir = meta.is_dir();
                Ok(Self::fattr(
                    id,
                    is_dir,
                    listing::exec_of(&meta),
                    if is_dir { 0 } else { meta.len() },
                    listing::mtime_of(&meta),
                ))
            }
            // 篮子根没有磁盘对应物，但它也有一个真实的时间：篮子定义那个文件的 mtime。
            None => Ok(Self::fattr(id, true, false, 0, self.svc.baskets_mtime())),
        }
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let (basket, sub) = self.target(id).await?;
        let Some((path, meta)) = Self::disk_path(&basket, &sub).await? else {
            // 篮子根是目录。
            return Err(nfsstat3::NFS3ERR_ISDIR);
        };
        if meta.is_dir() {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, bool)> {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::open(&path)?;
            let len = file.metadata()?.len();
            // 先算准要读多少，免得读越过 EOF（也免得返回假的 eof 标志）。
            let want = len.saturating_sub(offset).min(u64::from(count)) as usize;
            let mut buf = vec![0u8; want];
            let mut done = 0;
            // pread 允许短读，循环补齐。
            while done < want {
                let n = file.read_at(&mut buf[done..], offset + done as u64)?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            buf.truncate(done);
            Ok((buf, offset + done as u64 >= len))
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .map_err(|_| nfsstat3::NFS3ERR_IO)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let (basket, sub) = self.target(dirid).await?;
        let entries = Self::listing(&basket, &sub).await?;

        // 分页的 cookie 就是 fileid，所以子项必须按 fileid 有序。每次现排：目录不大，
        // 而这样不用维护一棵会过期的树。
        let mut ids = self.ids.lock().unwrap();
        let mut items: Vec<(fileid3, DirEntry)> = Vec::with_capacity(entries.len());
        for e in entries {
            let child = if sub.is_empty() {
                e.name.clone()
            } else {
                format!("{sub}/{}", e.name)
            };
            let fid = ids.intern((basket.id, child));
            let attr = Self::fattr(
                fid,
                matches!(e.kind, CherryKind::Dir),
                e.exec,
                e.size,
                e.mtime,
            );
            items.push((
                fid,
                DirEntry {
                    fileid: fid,
                    name: e.name.as_bytes().into(),
                    attr,
                },
            ));
        }
        items.sort_by_key(|(fid, _)| *fid);
        drop(ids);

        let start = items.partition_point(|(fid, _)| *fid <= start_after);
        let take = items.len() - start.min(items.len());
        let take = take.min(max_entries);
        let end = start + take >= items.len();
        Ok(ReadDirResult {
            entries: items[start..start + take]
                .iter()
                .map(|(_, d)| DirEntry {
                    fileid: d.fileid,
                    name: d.name.clone(),
                    attr: d.attr,
                })
                .collect(),
            end,
        })
    }

    // 下面是写操作。只读导出，一律拒。trait 要求全部实现，没有默认。
    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(&self, _dirid: fileid3, _filename: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    /// 篮子里的东西不会是符号链接（listing 只认文件和目录），所以客户端不该问。
    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_INVAL)
    }
}

/// 挂载路径里那段 32 位的十六进制。写法由 [`cherry_core::BasketLink::basket_id_hex`]
/// 定义，这里只负责认回来。
fn parse_id(hex_str: &str) -> Option<[u8; 16]> {
    cherry_core::BasketLink::parse_basket_id_hex(hex_str)
}

/// 只给单测用：把 id 转成挂载路径里的那一段。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同一个路径永远拿到同一个_fileid() {
        let mut a = Arena::default();
        let k = ([0u8; 16], "sub/file".to_owned());
        let first = a.intern(k.clone());
        assert_eq!(a.intern(k.clone()), first);
        assert_ne!(first, 0, "0 要留给全局根");
        assert_eq!(a.intern(([0u8; 16], String::new())), first + 1);
    }

    #[test]
    fn 只读但带执行位() {
        // 目录：能读能进。
        assert_eq!(BasketNfs::perm(true, false), 0o555);
        assert_eq!(BasketNfs::perm(true, true), 0o555);
        // 文件：可读不可写，执行位照实报。
        assert_eq!(BasketNfs::perm(false, false), 0o444);
        assert_eq!(BasketNfs::perm(false, true), 0o555);
        for p in [0o555, 0o444] {
            assert_eq!(p & 0o222, 0, "写位必须一个都不剩：{p:o}");
        }
    }
}
