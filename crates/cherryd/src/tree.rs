//! 把篮子里的一个目录打成 zip，边遍历边往外写。
//!
//! 安全规则只有一条，但必须守住：**不跟符号链接**。`DirEntry::file_type()` 是 lstat
//! 语义，所以这里从头到尾没有一次「跟随」。起点是已经校验过的篮子内路径，往下走就再也
//! 出不去——比每条路径各自 `canonicalize` 更不容易写错。
//!
//! 第二个前提：**篮子是活索引**，走到一半文件可能被删掉、被换掉、被截短。这些一律跳过
//! 并记日志，不让整条流断掉：对方拿到一个坏 zip，比少一个文件更糟。ADR-0001 的「不静默
//! 失败」在这里靠日志兑现，不靠状态码——HTTP 头早就发出去了，改不了。
//!
//! 因为不知道每个文件最终多大，大小和 CRC 都写在条目尾部的 data descriptor 里
//! （`ZipWriter::new_stream` 不需要 `Seek`）。副产品是「读到的字节数跟 stat 对不上」也
//! 不会把包弄坏。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;
use zip::write::SimpleFileOptions;
use zip::write::StreamWriter;
use zip::{CompressionMethod, ZipWriter};

/// 流式的那种 zip writer：`StreamWriter` 底下只要求 `Write`（它自己的 `Seek` 只往前，
/// 往回退会报错），所以整个包里不会有一次真的 seek。
type StreamZip<W> = ZipWriter<StreamWriter<W>>;

/// 压缩级别。定它的经过（三份语料 × 三档后端的曲线）见 docs/performance.md。
const LEVEL: i64 = 2;

const COPY_BUF: usize = 64 * 1024;

/// 算一个目录多大时的预算。
///
/// 跑这玩意的机器不是服务器，是**用户自己的电脑**（同一台还在给别人做 NFS），
/// 所以宁可算不完也不能算很久。条目数是主闸（确定性的，好推理），
/// 时间是兜底（冷缓存、外置盘、机械盘那种情况）。
pub const SIZE_MAX_ENTRIES: u64 = 10_000;
pub const SIZE_MAX_MILLIS: u64 = 500;

/// 为什么停下了。得分开说：超条目说「文件太多」是对，超时间说「文件太多」就是瞎话
/// （冷着的外置盘上三百个文件也会被时间闸切掉）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    #[default]
    Done,
    TooMany,
    TooSlow,
}

/// 一个目录的总账。`bytes` 是里面所有文件的字节数之和（不含目录本身）。
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Sized {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub stop: Stop,
}

impl Sized {
    fn entries(&self) -> u64 {
        self.files + self.dirs
    }
}

/// 走一遍目录算总大小。只 stat，不读文件内容；不跟符号链接（跟打包同一套规则）。
///
/// 用显式栈而不是递归：树深没上限，深到一万层的递归会把阻塞线程的栈弄爆。
pub fn size_of(root: &Path) -> Sized {
    let mut acc = Sized::default();
    let started = Instant::now();
    let mut stack = vec![root.to_path_buf()];
    let mut ticks: u64 = 0;

    while let Some(dir) = stack.pop() {
        // 读不出来的目录跳过：算大小不是关键路径，不该因为一个权限问题就把整件事报错。
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for de in rd.flatten() {
            if acc.entries() >= SIZE_MAX_ENTRIES {
                acc.stop = Stop::TooMany;
                return acc;
            }
            // 每 256 项看一眼表：`Instant::now()` 不贵，但也不必每一项都调。
            ticks += 1;
            if ticks.is_multiple_of(256) && started.elapsed().as_millis() as u64 >= SIZE_MAX_MILLIS
            {
                acc.stop = Stop::TooSlow;
                return acc;
            }

            // `file_type()` 在 APFS 上直接读 readdir 给的 d_type，不额外 stat。
            let Ok(ft) = de.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                tracing::warn!(path = %de.path().display(), "符号链接，不算进大小");
            } else if ft.is_dir() {
                acc.dirs += 1;
                stack.push(de.path());
            } else if ft.is_file() {
                // 字节数只能 stat 拿（readdir / d_type 不带这个）。
                match fs::symlink_metadata(de.path()) {
                    Ok(m) => {
                        acc.files += 1;
                        acc.bytes += m.len();
                    }
                    // 列出来和 stat 之间没了：跳过，不是失败。
                    Err(_) => continue,
                }
            }
        }
    }
    acc
}

/// 这些东西本来就是压过的，deflate 白烧 CPU——实测每档级别的压缩率都是 1.00，1 级偶尔
/// 还涨一点点。清单和判据见 docs/performance.md。
///
/// 认不出来的一律让它压：漏掉的代价只是多烧点 CPU，认错的代价是传的字节变多。
pub fn already_compressed(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    matches!(
        ext.to_ascii_lowercase().as_str(),
        // 图片
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "heic" | "heif" | "jxl"
        // 相机 raw：里面装的也是压过的数据（dng 可能是未压的 TIFF，所以不收）
        | "cr2" | "cr3" | "nef" | "arw" | "orf" | "raf" | "rw2"
        // 视频
        | "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "mpg" | "mpeg"
        | "m2ts" | "mts" | "wmv" | "flv" | "3gp" | "ogv"
        // 音频
        | "mp3" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "flac" | "wma" | "ape" | "wv"
        // 压缩包和镜像
        | "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "jar"
        | "war" | "apk" | "ipa" | "xpi" | "crx" | "cbz" | "dmg" | "iso" | "pdf"
        // 本身就是 zip 容器的文档
        | "docx" | "xlsm" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub"
        | "pages" | "numbers" | "keynote" | "sketch"
        // 字体
        | "woff" | "woff2"
    )
}

/// 一次打包数到的账。`bytes` 是**读进去的原始字节**，不是压完的大小。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Packed {
    pub files: u64,
    pub bytes: u64,
    /// 跳过的条目（符号链接、半路没了、读不出来的）。
    pub skipped: u64,
}

impl Packed {
    pub fn mib(&self) -> f64 {
        self.bytes as f64 / 1048576.0
    }
}

/// 打一个目录：包内先套一层同名目录（跟 Finder 的「压缩」一个样）。
///
/// `out` 只要求 `Write`：全程不会有任何一次 seek，所以可以直接接 HTTP 响应体。
pub fn write_zip_dir(root: &Path, wrapper: &str, out: &mut dyn Write) -> Result<Packed> {
    let mut z = ZipWriter::new_stream(out);
    let mut acc = Packed::default();
    // 顶层目录自己也写一条，空目录才留得住。
    let opts = store_options(meta_of(root).as_ref().map(cherry_core::listing::mtime_of));
    z.add_directory(format!("{wrapper}/"), opts)
        .with_context(|| format!("在 zip 里开目录 `{wrapper}/` 失败"))?;
    add_dir(&mut z, root, wrapper, &mut acc)?;
    z.finish().context("收尾 zip 失败（响应大概已经断了）")?;
    Ok(acc)
}

/// 打整个篮子。
///
/// 篮子根**不是一个目录**，是一排 cherry 并排（见 ADR-0002），所以这里没有那一层外套：
/// 每颗 cherry 各自是包根上的一条。
pub fn write_zip_roots(roots: &[(String, PathBuf)], out: &mut dyn Write) -> Result<Packed> {
    let mut z = ZipWriter::new_stream(out);
    let mut acc = Packed::default();
    for (name, path) in roots {
        let Some(meta) = meta_of(path) else {
            // 引用没了。跳过，日志留一条；文件里那一行不动（ADR-0008）。
            tracing::warn!(cherry = %name, "源没了，打包跳过");
            acc.skipped += 1;
            continue;
        };
        add_entry(&mut z, path, name, meta.file_type(), &mut acc)?;
    }
    z.finish().context("收尾 zip 失败（响应大概已经断了）")?;
    Ok(acc)
}

/// 一个条目：`inside` 是它在包内的完整名字。
fn add_entry<W: Write>(
    z: &mut StreamZip<W>,
    path: &Path,
    inside: &str,
    ft: fs::FileType,
    acc: &mut Packed,
) -> Result<()> {
    if ft.is_symlink() {
        // 跟 `listing::servable` 同一条规则：链接指向哪儿都不打进去。
        tracing::warn!(path = %path.display(), "符号链接，打包跳过");
        acc.skipped += 1;
    } else if ft.is_dir() {
        let opts = store_options(meta_of(path).as_ref().map(cherry_core::listing::mtime_of));
        z.add_directory(format!("{inside}/"), opts)
            .with_context(|| format!("在 zip 里开目录 `{inside}/` 失败"))?;
        add_dir(z, path, inside, acc)?;
    } else if ft.is_file() {
        add_file(z, path, inside, acc)?;
    } else {
        tracing::warn!(path = %path.display(), "既不是文件也不是目录，打包跳过");
        acc.skipped += 1;
    }
    Ok(())
}

fn add_dir<W: Write>(
    z: &mut StreamZip<W>,
    dir: &Path,
    prefix: &str,
    acc: &mut Packed,
) -> Result<()> {
    let rd = fs::read_dir(dir).with_context(|| format!("读不到 {}", dir.display()))?;
    let mut items: Vec<(String, PathBuf, fs::FileType)> = Vec::new();
    for de in rd {
        let de = match de {
            Ok(de) => de,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), "目录项读不出来，打包跳过: {e:#}");
                acc.skipped += 1;
                continue;
            }
        };
        let name = match de.file_name().into_string() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(path = %de.path().display(), "名字不是 UTF-8，打包跳过");
                acc.skipped += 1;
                continue;
            }
        };
        let ft = match de.file_type() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %de.path().display(), "类型读不出来，打包跳过: {e:#}");
                acc.skipped += 1;
                continue;
            }
        };
        items.push((name, de.path(), ft));
    }
    // 顺序跟列表一致：名字升序。对方解开之后看着也顺。
    items.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path, ft) in items {
        let inside = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        add_entry(z, &path, &inside, ft, acc)?;
    }
    Ok(())
}

fn add_file<W: Write>(
    z: &mut StreamZip<W>,
    path: &Path,
    inside: &str,
    acc: &mut Packed,
) -> Result<()> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), "打开失败，打包跳过: {e:#}");
            acc.skipped += 1;
            return Ok(());
        }
    };
    let mtime = meta_of(path).as_ref().map(cherry_core::listing::mtime_of);
    let opts = if already_compressed(inside) {
        store_options(mtime)
    } else {
        deflate_options(mtime)
    };
    z.start_file(inside, opts)
        .with_context(|| format!("在 zip 里开条目 `{inside}` 失败"))?;

    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            // 读到一半没了/坏了：这个条目就到这里，包本身还是好的——大小和 CRC 是
            // 条目收尾时才写的，写多少算多少。日志里留一条。
            Err(e) => {
                tracing::warn!(path = %path.display(), "读到一半失败，这个文件就这些了: {e:#}");
                acc.skipped += 1;
                break;
            }
        };
        // 这里失败一般是对方断了。往上抛，由调用方记一条。
        z.write_all(&buf[..n]).context("往响应里写失败")?;
        acc.bytes += n as u64;
    }
    acc.files += 1;
    Ok(())
}

fn deflate_options(mtime: Option<u64>) -> SimpleFileOptions {
    with_mtime(
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(LEVEL)),
        mtime,
    )
}

fn store_options(mtime: Option<u64>) -> SimpleFileOptions {
    with_mtime(
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        mtime,
    )
}

fn with_mtime(o: SimpleFileOptions, mtime: Option<u64>) -> SimpleFileOptions {
    match mtime.and_then(zip_time) {
        Some(t) => o.last_modified_time(t),
        None => o,
    }
}

/// Unix 秒 → zip 的 DOS 时间戳，用**本地时间**。
///
/// 对方解开时看到该是文件原来那个「墙上的时间」，不是 UTC。zip 那个格式只到 1980~2107，
/// 超出范围（或者 stat 不出来）就放弃，让它落到默认值。
fn zip_time(secs: u64) -> Option<zip::DateTime> {
    // 类型靠 `localtime_r` 的签名推出来，不写 `libc::time_t`——那个别名在 musl 1.2 之后
    // 被标了 deprecated（32 位还是 32 位、64 位才是 64 位），写出来就吃一条警告。
    let t = secs as _;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // `localtime_r` 把结果写进我们给的 tm 里，不返回任何共享状态。
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    zip::DateTime::from_date_and_time(
        tm.tm_year as u16 + 1900,
        tm.tm_mon as u8 + 1,
        tm.tm_mday as u8,
        tm.tm_hour as u8,
        tm.tm_min as u8,
        tm.tm_sec as u8,
    )
    .ok()
}

fn meta_of(path: &Path) -> Option<fs::Metadata> {
    // `symlink_metadata`：这里也不跟链接。
    fs::symlink_metadata(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 每个测试拿自己的目录：并行跑时不能互相删。
    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cherry-tree-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("子目录/空的")).unwrap();
        fs::write(dir.join("文本.txt"), b"hello".repeat(100)).unwrap();
        // 名字像 png，内容其实是文本——按扩展名判，它就该被存起来。
        fs::write(dir.join("假图片.png"), b"not really a png").unwrap();
        fs::write(dir.join("里面.bin"), b"x").unwrap();
        dir
    }

    #[test]
    fn 压过的直接存没压过的才压() {
        assert!(already_compressed("照片.JPG"), "大写扩展名也算");
        assert!(already_compressed("包.tar.gz"), "看最后一段");
        assert!(already_compressed("片子.mp4"));
        assert!(!already_compressed("笔记.md"));
        assert!(
            !already_compressed("源码.ts"),
            "ts 多半是 TypeScript，不是 MPEG-TS"
        );
        assert!(!already_compressed("没扩展名"));
        assert!(!already_compressed("未压的.tiff"));
    }

    #[test]
    fn 打出来的包能读回来() {
        let dir = tree("roundtrip");
        let mut out = Vec::new();
        let acc = write_zip_dir(&dir, "share", &mut out).unwrap();

        assert_eq!(acc.files, 3, "三个真文件");
        assert_eq!(acc.skipped, 0);
        assert_eq!(acc.bytes, 500 + 16 + 1);

        let mut z = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        let names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        assert!(
            names.contains(&"share/".to_owned()),
            "顶层目录自己也要有条目"
        );
        assert!(names.contains(&"share/子目录/".to_owned()));
        assert!(
            names.contains(&"share/子目录/空的/".to_owned()),
            "空目录要留得住: {names:?}"
        );

        let mut text = String::new();
        z.by_name("share/文本.txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello".repeat(100));

        let stored = z.by_name("share/假图片.png").unwrap().compression();
        assert_eq!(stored, CompressionMethod::Stored, "压过的该原样存");
        let deflated = z.by_name("share/文本.txt").unwrap().compression();
        assert_eq!(deflated, CompressionMethod::Deflated, "能压的该压");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 符号链接不进去——跟「共享的只有真文件和真目录」是同一条规则。
    #[cfg(unix)]
    #[test]
    fn 包里的符号链接不进() {
        let dir = tree("symlink");
        let outside = dir.with_extension("outside");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("链接")).unwrap();

        let mut out = Vec::new();
        let acc = write_zip_dir(&dir, "share", &mut out).unwrap();
        assert_eq!(acc.skipped, 1);
        let mut z = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        assert!(z.by_name("share/链接").is_err(), "链接不该在包里");

        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 算大小：只 stat，算出来的字节跟打包/拷出来的一致。
    #[test]
    fn 算得对而且不跟符号链接() {
        let dir = tree("size");
        let outside = dir.with_extension("outside-size");
        fs::write(&outside, b"0123456789").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("链接")).unwrap();

        let s = size_of(&dir);
        assert_eq!(s.stop, Stop::Done);
        assert_eq!(s.files, 3);
        assert_eq!(s.dirs, 2, "子目录和里面那个空的");
        assert_eq!(s.bytes, 500 + 16 + 1, "外面的那个不算，链接也不算");

        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 树太大就停下并如实说是哪一条闸() {
        let dir = tree("toomany");
        // 用目录顶条目：建目录比建文件便宜。预算算的是「条目」，文件和目录一视同仁
        // （都要 stat），所以这一棵树就到闸了。
        let many = dir.join("many");
        fs::create_dir_all(&many).unwrap();
        for i in 0..SIZE_MAX_ENTRIES {
            fs::create_dir(many.join(format!("d{i}"))).unwrap();
        }

        let s = size_of(&dir);
        assert_eq!(s.stop, Stop::TooMany, "过了一万项就该停，不能接着走");
        assert!(s.entries() >= SIZE_MAX_ENTRIES);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 打整个篮子：篮子根是一排 cherry，没有外套那层。
    #[test]
    fn 打整个篮子时顶层条目并排() {
        let dir = tree("roots");
        let roots = vec![
            ("文本.txt".to_owned(), dir.join("文本.txt")),
            ("子目录".to_owned(), dir.join("子目录")),
        ];
        let mut out = Vec::new();
        write_zip_roots(&roots, &mut out).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(out)).unwrap();
        assert!(z.by_name("文本.txt").is_ok());
        assert!(z.by_name("子目录/空的/").is_ok(), "空目录也在");
        assert!(z.by_name("share/文本.txt").is_err(), "不该有外套那层");

        let _ = fs::remove_dir_all(&dir);
    }
}
