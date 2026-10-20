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
//!
//! **句柄跨重启稳定。** fileid 是路径的哈希，句柄就用它；篮子根额外一种形态：把 128 位
//! 篮子 id 直接放进句柄。这样服务重启之后，对方**已经挂着的**那个点不用重挂——要么照常，
//! 要么个别句柄报一次 `STALE`，客户端按名字重新 lookup 就回来了。理由见 [`BasketNfs::id_to_fh`]。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use cherry_core::listing::{self, Entry};
use cherry_core::{Basket, CherryKind, Service};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfs_fh3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
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
/// **fileid 是路径的哈希，不是进程内自增的计数器。** 这一条是为了让服务重启之后客户端
/// 手里的句柄还有意义：自增的话新进程会从头重发同样的号，而客户端缓存里那些号指的是别的
/// 路径，于是老句柄会**悄悄解析成另一个文件**（读 `bin/` 拿到 `errors.md` 的内容）——
/// 这比报错糟得多。哈希让同一个路径在任何进程里都是同一个号：老句柄要么解析到同一个
/// 对象，要么解析不了报 `STALE`，绝不会指到别人身上。
#[derive(Default)]
struct Arena {
    by_id: HashMap<fileid3, Key>,
    by_key: HashMap<Key, fileid3>,
}

/// 篮子的 128 位 id，加上篮子内的相对路径。空串表示篮子根。
type Key = ([u8; 16], String);

impl Arena {
    fn intern(&mut self, key: Key) -> Result<fileid3, nfsstat3> {
        if let Some(&id) = self.by_key.get(&key) {
            return Ok(id);
        }
        let id = hash_key(&key);
        // 撞了不能糊过去：一个号指向两个路径的话，客户端拿到的是别人。64 位哈希上
        // 这不该发生，真发生了要响亮。
        if let Some(other) = self.by_id.get(&id) {
            tracing::error!(id, ?key, ?other, "fileid 撞了，这个路径先不给");
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        self.by_key.insert(key.clone(), id);
        self.by_id.insert(id, key);
        Ok(id)
    }

    fn key(&self, id: fileid3) -> Option<&Key> {
        self.by_id.get(&id)
    }
}

/// 路径 → fileid：FNV-1a，64 位。
///
/// 自己写一个固定哈希而不是用 `DefaultHasher`：后者只保证同一个二进制里一致，
/// Rust 一升级就可能换实现，那样连篮子根的句柄都会失效。
fn hash_key((basket_id, sub): &Key) -> fileid3 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    // 在 id 和路径之间插一个 0：免得出"id 的尾巴拼上路径"和"另一个 id 加上另一条路径"撞到一起。
    let bytes = basket_id
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .chain(sub.bytes());
    for b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    // 0 留给"全局根"那个占位（nfsserve 的约定），别让它落在这儿。
    if h == 0 { 1 } else { h }
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

    /// 篮子根没有磁盘对应物，给它一个普通的目录权限位。
    ///
    /// 写不写得了不靠这个数字：写操作一律报 `NFS3ERR_ROFS`（见下面那一串），挂载命令里
    /// 也带 `ro`。
    const ROOT_MODE: u32 = 0o755;

    /// `mode` 是**照实报**的源文件权限位，见 [`listing::mode_of`]。
    fn fattr(id: fileid3, is_dir: bool, mode: u32, size: u64, mtime: u64) -> fattr3 {
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
            mode,
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

    /// 句柄就是 fileid 本身；只有篮子根例外，它把自己的 128 位 id 放进去。
    ///
    /// **不用 nfsserve 默认那一套**：默认往句柄里塞一个「每进程生成一次」的 generation
    /// number（`SystemTime::now().as_millis()`），所以服务一重启，客户端手里的句柄全部作废，
    /// 报 `STALE`，对方得重新挂载。我们的 fileid 是路径哈希，本来就跨进程稳定，不需要那个。
    ///
    /// **篮子根为什么要单独一种形态**：新进程里谁都还没 intern 过它，而它是唯一一个**必须**
    /// 能被「冷」解析的——客户端够任何东西、走哪条路径，都从这个句柄开始。而哈希不可逆，我们
    /// 没法从 64 位反推出是哪个篮子；干脆把篮子 id 本身放进去（它本来就在挂载路径里明文传，
    /// 不算多泄露）。
    ///
    /// 剩下的缺口：非根句柄冷着拿过来只能报 `STALE`，因为同样反推不出来。实践中客户端会按
    /// 名字重新 lookup（名字→句柄的缓存寿命是挂载时的 `acdirmax`），所以顶多错一下；但**已经
    /// 打开的文件**手里那个句柄是跑不掉的，那边的 `cp`/`rsync` 得重来。
    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let basket_root = {
            let ids = self.ids.lock().unwrap();
            ids.key(id)
                .and_then(|(b, sub)| sub.is_empty().then_some(*b))
        };
        let data = match basket_root {
            Some(basket_id) => basket_id.to_vec(),
            None => id.to_le_bytes().to_vec(),
        };
        nfs_fh3 { data }
    }

    /// [`BasketNfs::id_to_fh`] 的反函数。
    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        match fh.data.len() {
            // 篮子根：把 key 造出来，intern 一下就得到那个（稳定的）fileid。
            // 篮子不存在的话，后面真正处理请求时会报错，这里不管。
            16 => {
                let basket_id: [u8; 16] = fh.data[..].try_into().unwrap();
                self.ids.lock().unwrap().intern((basket_id, String::new()))
            }
            8 => {
                let id = fileid3::from_le_bytes(fh.data[..].try_into().unwrap());
                if self.ids.lock().unwrap().key(id).is_some() {
                    Ok(id)
                } else {
                    // 这条路径本进程还没见过。不去猜（也猜不出来），让客户端按名字重新 lookup。
                    Err(nfsstat3::NFS3ERR_STALE)
                }
            }
            _ => Err(nfsstat3::NFS3ERR_BADHANDLE),
        }
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
        Ok(self.ids.lock().unwrap().intern((id, sub.to_owned()))?)
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
        Ok(self.ids.lock().unwrap().intern((basket.id, child))?)
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
                    listing::mode_of(&meta),
                    if is_dir { 0 } else { meta.len() },
                    listing::mtime_of(&meta),
                ))
            }
            // 篮子根没有磁盘对应物，但它也有一个真实的时间：定义它的那个文件的 mtime。
            None => Ok(Self::fattr(id, true, Self::ROOT_MODE, 0, basket.mtime())),
        }
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
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
            let fid = ids.intern((basket.id, child))?;
            let attr = Self::fattr(
                fid,
                matches!(e.kind, CherryKind::Dir),
                e.mode,
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
        let first = a.intern(k.clone()).unwrap();
        assert_eq!(a.intern(k.clone()).unwrap(), first, "同一个路径同一个号");
        assert_ne!(first, 0, "0 要留给全局根");
        // 换个路径、换个篮子都得换号。
        assert_ne!(a.intern(([0u8; 16], String::new())).unwrap(), first);
        assert_ne!(a.intern(([1u8; 16], "sub/file".to_owned())).unwrap(), first);
        assert_eq!(a.key(first), Some(&k));
    }

    #[test]
    fn id_是路径的哈希_所以重启后还是同一个() {
        // 这条是整套设计的全部理由：换一个进程（新的 Arena）算出来必须一样。
        // 要是自增的话，新进程会把同一个号发给另一条路径，客户端手里的老句柄就会静默指错。
        let k = ([7u8; 16], "bin/cherry".to_owned());
        let mut a = Arena::default();
        let mut b = Arena::default();
        assert_eq!(a.intern(k.clone()).unwrap(), b.intern(k).unwrap());
    }

    #[tokio::test]
    async fn 篮子根的句柄换个进程还认得() {
        let dir = std::env::temp_dir().join(format!("cherry-nfs-fh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let svc = Service::start(dir.clone()).await.unwrap();
        // 篮子就是数据目录里的一个文件（ADR-0008）。
        std::fs::write(dir.join("t.basket"), "~/Pictures\n").unwrap();
        let b = svc.basket("t").await.unwrap();
        let nfs = BasketNfs {
            svc: svc.clone(),
            ids: Mutex::new(Arena::default()),
        };

        let hex: String = b.id.iter().map(|x| format!("{x:02x}")).collect();
        let id = nfs.path_to_id(format!("/{hex}").as_bytes()).await.unwrap();
        let fh = nfs.id_to_fh(id);
        assert_eq!(fh.data, b.id.to_vec(), "篮子根的句柄里就是篮子 id");

        // 模拟服务重启：换一个空白的 Arena（新进程），拿同一个句柄过来。
        let fresh = BasketNfs {
            svc: svc.clone(),
            ids: Mutex::new(Arena::default()),
        };
        assert_eq!(fresh.fh_to_id(&fh).unwrap(), id, "重启之后还是同一个号");

        // 非根的冷句柄认不出来——不猜，报 STALE，让客户端按名字重新 lookup。
        let cold = nfs_fh3 {
            data: 12_345u64.to_le_bytes().to_vec(),
        };
        assert!(matches!(
            fresh.fh_to_id(&cold),
            Err(nfsstat3::NFS3ERR_STALE)
        ));
        // 长度不对的句柄是 BADHANDLE。
        assert!(matches!(
            fresh.fh_to_id(&nfs_fh3 {
                data: vec![1, 2, 3]
            }),
            Err(nfsstat3::NFS3ERR_BADHANDLE)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 权限照实报() {
        use std::os::unix::fs::PermissionsExt;

        // 磁盘上的文件什么样，报出去就什么样：`cp` 出来才是正常文件。
        let f = std::env::temp_dir().join(format!("cherry-mode-{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_eq!(listing::mode_of(&meta), 0o640);
        std::fs::remove_file(&f).unwrap();

        // 写不写得了不靠这些位：写操作一律 NFS3ERR_ROFS，挂载命令里带 `ro`。
        assert_eq!(BasketNfs::ROOT_MODE, 0o755);
        assert_eq!(
            listing::mode_of(&meta) & 0o111,
            0,
            "没有执行位的文件不该报出执行位"
        );
    }
}
