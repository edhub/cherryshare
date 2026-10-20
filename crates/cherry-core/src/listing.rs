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
//!
//! 可见路径可能有多段（ADR-0009）：中间层是**虚拟目录**，只在内存里，不对应磁盘上的
//! 某个目录；只有被点名的叶子才 `resolve` 到源文件。所以这里有两种目标：
//! [`Target::Virtual`] 和 [`Target::Real`]。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::basket::{Basket, Node};

/// 虚拟目录的权限位。它没有源文件，报一个普通的目录位；只读由写操作拒绝和 `ro` 保证。
pub const VIRTUAL_MODE: u32 = 0o755;

/// 一棵树里的一项：普通文件还是真目录。符号链接、设备、fifo 都不是这两个，不进共享。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CherryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 这个目录里的**叶子名**。完整路径由请求它的那个目录拼出来：`dir + "/" + name`。
    pub name: String,
    pub kind: CherryKind,
    /// 目录记 0。
    pub size: u64,
    /// Unix 秒。目录也填真实 mtime（见 [`entry`]）；虚拟目录用篮子文件的 mtime。
    pub mtime: u64,
    /// 源文件的权限位（低 12 位，含执行位和那几个特殊位）。
    ///
    /// **照实报**：这是导出给对方的描述，不是我们自己的策略。写不写得了由服务端拒
    /// （NFS3ERR_ROFS）和挂载时的 `ro` 决定，跟这个数字无关；而这个数字会被对方机器上的
    /// `cp`、`rsync -a`、`tar`、Finder 拖拽拿去当**目标文件**的权限——报成 `0o444` 的话，
    /// 从共享里拷出来的一切都是只读的（连带着目录是 `0o555`，往里面加文件都加不了）。
    pub mode: u32,
}

/// 篮子内的一条路径落在哪里。
pub enum Target<'a> {
    /// 虚拟目录：篮子根，或者 `/./` 造出来的中间层。磁盘上没有对应物。
    Virtual(&'a Node),
    /// 磁盘上的路径：正好是一颗 cherry，或者在这颗 cherry 里面。
    Real {
        source: &'a Path,
        /// 请求路径里落在 cherry 源路径之后的那几段。整颗 cherry 本身时为空。
        rest: Vec<String>,
    },
}

/// [`resolve`] 的结果。虚拟目录没有磁盘路径。
#[derive(Debug)]
pub enum Resolved {
    Disk(PathBuf, std::fs::Metadata),
    Virtual,
}

/// 列**一个**目录。`dir` 是篮子内的相对路径，空串表示篮子根（也就是可见路径的根）。
///
/// 虚拟目录只列点名的子项：一条死引用跳过，一颗真 cherry 照旧 stat；中间层直接报目录，
/// 不碰磁盘。真目录 cherry 和它里面的路径还是走 `read_dir`。
///
/// 虚拟目录这一层会把「引用失效」暴露成错误的地方：每颗 cherry 都要 `metadata`。
/// 做法是**跳过**，不是让整层失败——一条死引用不该让整个篮子打不开。文件里写着它、而源
/// 已经没了，这种事由 `cherry show` 标出来（ADR-0008：工具不替你改文件）。
///
/// **子目录不受影响**——这是按目录列带来的额外好处。
pub fn list_dir(basket: &Basket, dir: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();

    match target(basket, dir)? {
        Target::Virtual(node) => {
            for (name, child) in &node.children {
                match &child.source {
                    Some(source) => match entry(name, source) {
                        Ok(Some(e)) => out.push(e),
                        // 不会是 `None`：源路径在磁盘上是什么由列目录当场判；真出现就是
                        // 文件里那一行指到了符号链接之类的东西。
                        Ok(None) => {
                            tracing::warn!(cherry = %name, "不是普通文件也不是真目录，跳过")
                        }
                        // 源没了。文件里还写着它，但那是你的文件，工具不替你改（ADR-0008）；
                        // 跳过就行——一条死引用不该把整层拖垮。
                        Err(e) => tracing::warn!(cherry = %name, "源的元数据读不到，跳过: {e:#}"),
                    },
                    None => out.push(Entry {
                        name: name.clone(),
                        kind: CherryKind::Dir,
                        size: 0,
                        mtime: basket.mtime(),
                        mode: VIRTUAL_MODE,
                    }),
                }
            }
        }
        Target::Real { .. } => {
            let Resolved::Disk(full, meta) = resolve(basket, dir)? else {
                unreachable!("target 说是磁盘路径，resolve 不会说虚拟")
            };
            if !meta.is_dir() {
                bail!("`{dir}` 不是一个目录");
            }
            let rd =
                std::fs::read_dir(&full).with_context(|| format!("读不到 {}", full.display()))?;
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
                    Err(e) => {
                        tracing::warn!(path = %de.path().display(), "读不到元数据，跳过: {e:#}")
                    }
                }
            }
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 篮子内的相对路径落在虚拟目录还是磁盘路径上。
///
/// 先走可见路径的树；碰到一颗真 cherry 就把剩下的段还回去——那几段要落到磁盘上查。
fn target<'a>(basket: &'a Basket, path: &str) -> Result<Target<'a>> {
    let mut node = basket.tree();
    if path.is_empty() {
        return Ok(Target::Virtual(node));
    }

    let segs = split_path(path)?;
    for (i, seg) in segs.iter().enumerate() {
        if let Some(source) = &node.source {
            return Ok(Target::Real {
                source: source.as_path(),
                rest: segs[i..].iter().map(|s| (*s).to_owned()).collect(),
            });
        }
        let child = node
            .children
            .get(*seg)
            .with_context(|| format!("`{path}` 不在这个 basket 里"))?;
        node = child;
    }

    match &node.source {
        Some(source) => Ok(Target::Real {
            source: source.as_path(),
            rest: Vec::new(),
        }),
        None => Ok(Target::Virtual(node)),
    }
}

/// 路径必须是篮子内的相对路径，每一段都得是普通名字。
fn split_path(path: &str) -> Result<Vec<&str>> {
    let segs: Vec<&str> = path.split('/').collect();
    for seg in &segs {
        if seg.is_empty() || *seg == "." || *seg == ".." {
            bail!("路径里的 `{seg}` 不合法");
        }
    }
    Ok(segs)
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
/// 落到虚拟目录上的返回 [`Resolved::Virtual`]：它没有磁盘对应物，调用方自己知道怎么列。
///
/// 这是安全边界：`..`、绝对路径、以及**任何一段是符号链接**都不允许，否则查看者
/// 能读到 cherry 之外的东西。
///
/// 顺手把元数据带回来，并且在这里就把「能不能对外」判掉：调用方要判存在、判类型、
/// 要 size/mtime，全由这一次 stat 给出，别再各自 stat 一遍然后说法不一。
pub fn resolve(basket: &Basket, path: &str) -> Result<Resolved> {
    let Target::Real { source, rest } = target(basket, path)? else {
        return Ok(Resolved::Virtual);
    };

    let mut full = source.to_path_buf();
    for seg in &rest {
        full.push(seg.as_str());
    }

    // 段里没有 `..` 不等于爬不出去：中间任何一段都可以是符号链接。
    //
    // 判法是「把解析后的真实路径的尾段，和请求的尾段逐段比」。只要中间跟过任何
    // 一个符号链接，两者就不一样，直接拒——这也正是「不共享符号链接」这条规则本身。
    //
    // 比较得在同一基准上：源路径是手写的，中间可能带符号链接（macOS 上 `/tmp`、
    // `/var` 就是这样），所以源也要规范化一次。源自己是个符号链接的先拒掉——列目录
    // 那边本来也会跳过它。
    //
    // macOS 上 `realpath` 保留传入的大小写，所以同一段名字换大小写不会被误拒。
    let root_meta = std::fs::symlink_metadata(source)
        .with_context(|| format!("读不到 {}", source.display()))?;
    servable(&root_meta)
        .with_context(|| format!("{} 不是普通文件也不是真目录，不共享", source.display()))?;
    let root = source
        .canonicalize()
        .with_context(|| format!("读不到 {}", source.display()))?;
    let real = full
        .canonicalize()
        .with_context(|| format!("读不到 {}", full.display()))?;
    let asked = full.strip_prefix(source).unwrap_or(Path::new(""));
    let got = real
        .strip_prefix(&root)
        .with_context(|| format!("`{path}` 指到了共享范围外面"))?;
    if got != asked {
        bail!("`{path}` 中间有符号链接，不共享");
    }

    let meta = std::fs::symlink_metadata(&full)
        .with_context(|| format!("stat {} 失败", full.display()))?;
    servable(&meta).with_context(|| format!("`{path}` 不是普通文件也不是真目录，不共享"))?;
    Ok(Resolved::Disk(full, meta))
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
    use crate::basket::Cherry;

    /// 每个测试拿自己的目录：用 `process::id()` 的话几个测试会并行地互相删掉对方的目录。
    fn tree(tag: &str) -> (Basket, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cherry-listing-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        // 云盘上 `/var` 是 `/private/var` 的符号链接：不 resolve 的话 `resolve` 会把
        // 每一段都对不上。服务那头也是拿规范化过的路径进篮子的。
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("ok.txt"), b"x").unwrap();
        std::fs::write(dir.join("sub/two.bin"), b"yy").unwrap();
        std::fs::write(dir.join("sub/deep/three.bin"), b"zzz").unwrap();

        let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let b = Basket::new(
            [0u8; 16],
            "t".into(),
            dir.join("t.basket"),
            vec![Cherry {
                path: name,
                source: dir.clone(),
                line: 1,
            }],
        )
        .unwrap();
        (b, dir)
    }

    /// 一串可见路径造一个篮子。`t.basket` 本身不存在，虚拟目录的 mtime 就是 0。
    fn basket(dir: &Path, cherries: Vec<Cherry>) -> Basket {
        Basket::new([0u8; 16], "t".into(), dir.join("t.basket"), cherries).unwrap()
    }

    #[test]
    fn 拒绝越界路径() {
        let (b, _d) = tree("resolve");
        let root = b.cherries[0].path.clone();
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
        let root = b.cherries[0].path.clone();
        let top = list_dir(&b, "").unwrap();
        assert_eq!(top.len(), 1, "根这一层只该有 cherry 本身");
        assert_eq!(top[0].name, root);
        assert_eq!(top[0].kind, CherryKind::Dir);
    }

    #[test]
    fn 列一个目录不递归() {
        let (b, _d) = tree("onelevel");
        let root = b.cherries[0].path.clone();
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
        let root = b.cherries[0].path.clone();
        assert!(list_dir(&b, &format!("{root}/nope")).is_err());
    }

    #[test]
    fn 对文件列目录要报错而不是当空目录() {
        let (b, _d) = tree("notdir");
        let root = b.cherries[0].path.clone();
        assert!(list_dir(&b, &format!("{root}/ok.txt")).is_err());
    }

    /// `/./` 造出来的中间层只列点名的子项，不把源目录里没写的东西带出来。
    #[test]
    fn 虚拟目录只列点名的子项() {
        let (_, dir) = tree("virtual");
        let b = basket(
            &dir,
            vec![
                Cherry {
                    path: "agent/APPEND_SYSTEM.md".into(),
                    source: dir.join("ok.txt"),
                    line: 1,
                },
                Cherry {
                    path: "agent/extensions/llm-timer.ts".into(),
                    source: dir.join("sub/two.bin"),
                    line: 2,
                },
                Cherry {
                    path: "agent/skills/wan".into(),
                    source: dir.join("sub/deep"),
                    line: 3,
                },
            ],
        );

        let top = list_dir(&b, "").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].name, "agent");
        assert_eq!(top[0].kind, CherryKind::Dir);
        assert_eq!(top[0].size, 0);
        assert_eq!(top[0].mode, VIRTUAL_MODE);

        let names: Vec<_> = list_dir(&b, "agent")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["APPEND_SYSTEM.md", "extensions", "skills"]);

        let ext: Vec<_> = list_dir(&b, "agent/extensions")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(ext, vec!["llm-timer.ts"]);

        let skills: Vec<_> = list_dir(&b, "agent/skills")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(skills, vec!["wan"]);

        let deep: Vec<_> = list_dir(&b, "agent/skills/wan")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(deep, vec!["three.bin"]);

        assert!(matches!(resolve(&b, "agent"), Ok(Resolved::Virtual)));
        assert!(matches!(
            resolve(&b, "agent/extensions"),
            Ok(Resolved::Virtual)
        ));
        assert!(matches!(
            resolve(&b, "agent/extensions/llm-timer.ts"),
            Ok(Resolved::Disk(..))
        ));
        assert!(matches!(
            resolve(&b, "agent/skills/wan"),
            Ok(Resolved::Disk(..))
        ));
        assert!(matches!(
            resolve(&b, "agent/skills/wan/three.bin"),
            Ok(Resolved::Disk(..))
        ));
    }

    /// 虚拟目录的 mtime 用篮子文件的 mtime；叶子还是源文件自己的。
    #[test]
    fn 虚拟目录的_mtime_用篮子文件() {
        let (_, dir) = tree("vmtime");
        let file = dir.with_extension("basket-mtime");
        std::fs::write(&file, "# t\n").unwrap();
        let b = Basket::new(
            [0u8; 16],
            "t".into(),
            file.clone(),
            vec![Cherry {
                path: "agent/x".into(),
                source: dir.join("ok.txt"),
                line: 1,
            }],
        )
        .unwrap();

        let top = list_dir(&b, "").unwrap();
        let want = crate::listing::mtime_of(&std::fs::metadata(&file).unwrap());
        assert_eq!(top[0].mtime, want);
        assert_eq!(b.mtime(), want);

        let _ = std::fs::remove_file(&file);
    }

    /// 虚拟目录里的一条死引用只跳过它自己；外壳还在，别的条目不受影响。
    #[test]
    fn 虚拟目录里源没了只跳过那一条() {
        let (_, dir) = tree("vdead");
        let dead = dir.join("dead.txt");
        std::fs::write(&dead, b"x").unwrap();
        let b = basket(
            &dir,
            vec![
                Cherry {
                    path: "agent/gone".into(),
                    source: dead.clone(),
                    line: 1,
                },
                Cherry {
                    path: "agent/ok".into(),
                    source: dir.join("ok.txt"),
                    line: 2,
                },
            ],
        );
        std::fs::remove_file(&dead).unwrap();

        let names: Vec<_> = list_dir(&b, "agent")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["ok"]);
        assert_eq!(list_dir(&b, "").unwrap().len(), 1, "外壳还在");
    }

    /// 符号链接不共享，也不许从它中间穿过：指到外面等于绕开 `..` 的封锁，指到里面
    /// 也不是一个真文件。两者都得拒。
    #[cfg(unix)]
    #[test]
    fn 符号链接既不共享也不许穿过() {
        let (b, dir) = tree("symlink");
        let root = b.cherries[0].path.clone();
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

    /// 只有真文件和真目录能对外。
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

    /// 源没了：跳过它就行，别让一条死引用把整个篮子根变成错误。文件里那一行不动
    /// （ADR-0008：工具不替你改文件）。
    #[test]
    fn 根这一层跳过源没了的() {
        let (b, dir) = tree("deadroot");
        let dead = dir.join("dead.txt");
        std::fs::write(&dead, b"x").unwrap();
        let mut cherries = b.cherries.clone();
        cherries.push(Cherry {
            path: "dead.txt".into(),
            source: dead.clone(),
            line: 2,
        });
        let b = basket(&dir, cherries);
        assert_eq!(list_dir(&b, "").unwrap().len(), 2);

        std::fs::remove_file(&dead).unwrap();
        let top = list_dir(&b, "").unwrap();
        assert_eq!(top.len(), 1, "死引用该被跳过，不是让整层失败");
        assert_eq!(top[0].name, b.cherries[0].path);
    }

    /// 篮子文件里的路径是手写的，中间可能带符号链接（macOS 上 `/tmp`、`/var` 就是）。
    /// 那不是穿过共享边界，不能因为源路径长这样就全读不到。
    #[cfg(unix)]
    #[test]
    fn 源路径中间有符号链接也能读() {
        let base = std::env::temp_dir().join(format!("cherry-viasym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/sub")).unwrap();
        std::fs::write(base.join("real/sub/a.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();

        let b = Basket::new(
            [0u8; 16],
            "t".into(),
            base.join("t.basket"),
            vec![Cherry {
                path: "sub".into(),
                source: base.join("link/sub"),
                line: 1,
            }],
        )
        .unwrap();
        assert!(resolve(&b, "sub").is_ok(), "源路径中间有链接不该拦");
        assert!(resolve(&b, "sub/a.txt").is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 源自己是个符号链接：不共享，子路径也不许绕过。
    #[cfg(unix)]
    #[test]
    fn 源本身是符号链接就拒() {
        let base = std::env::temp_dir().join(format!("cherry-selflink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::fs::write(base.join("real/a.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();

        let b = Basket::new(
            [0u8; 16],
            "t".into(),
            base.join("t.basket"),
            vec![Cherry {
                path: "link".into(),
                source: base.join("link"),
                line: 1,
            }],
        )
        .unwrap();
        assert!(resolve(&b, "link").is_err());
        assert!(resolve(&b, "link/a.txt").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
