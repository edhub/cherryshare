//! basket 与 cherry。
//!
//! 一个篮子就是数据目录里的一个 `<名字>.basket` 文件（ADR-0008）：一行一条要分享的路径，
//! `id` 一行是访问凭证。**文件是唯一真相**——服务只读它，唯一一处写入是补一个缺失的 `id`。
//!
//! 篮子不是一个目录：文件留在原处，这里只记录引用。见 ADR-0001。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 篮子文件的后缀。数据目录里其它的东西（编辑器备份、`.migrated`）一概不认。
pub const SUFFIX: &str = ".basket";

/// 篮子里的东西。放进来的是引用，源文件不动。
#[derive(Debug, Clone)]
pub struct Cherry {
    /// 在篮子里的名字：源路径的最后一段，也是 listing 路径的第一段。
    pub name: String,
    /// 创建者磁盘上的绝对路径。
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Basket {
    /// 128 bit 随机（ADR-0002）。需要的是不可猜测性，不是唯一性。
    pub id: [u8; 16],
    /// 文件名去掉 `.basket`。改了文件名就是改了篮子名，id 不变。
    pub name: String,
    /// 定义这个篮子的文件（`<数据目录>/<名字>.basket`）。
    pub file: PathBuf,
    pub cherries: Vec<Cherry>,
}

impl Basket {
    pub fn cherry(&self, name: &str) -> Option<&Cherry> {
        self.cherries.iter().find(|c| c.name == name)
    }

    /// 篮子根那个虚拟目录的 mtime：定义它的那个文件的 mtime。
    ///
    /// 篮子根在磁盘上没有对应物，但它得有一个**真的**时间，而「篮子的定义变了吗」正好有
    /// 对应物——改一行就碰一下这个文件，改源文件内容不会。
    pub fn mtime(&self) -> u64 {
        std::fs::metadata(&self.file)
            .map(|m| crate::listing::mtime_of(&m))
            .unwrap_or(0)
    }
}

/// 从文件读一个篮子。
///
/// 文件里没有 `id` 行就发一个并写回去——这是本程序对配置文件的唯一写入。写不回去就算
/// 这个篮子读不进来：拿一个只活在内存里的 id 去服务，重起之后链接会变，那比直接说
/// 「读不进来」更糟。
pub fn load(path: &Path) -> Result<Basket> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("{} 的文件名不是 UTF-8", path.display()))?
        .to_owned();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("读不到 {}", path.display()))?;
    let parsed = parse(&text)?;

    let id = match parsed.id {
        Some(id) => id,
        None => {
            let id = mint_id()?;
            write_id(path, &id)
                .with_context(|| format!("文件里没有 id，也写不回去 {}", path.display()))?;
            tracing::info!(basket = %name, file = %path.display(), "文件里没有 id，发了一个并写回去");
            id
        }
    };

    Ok(Basket {
        id,
        name,
        file: path.to_path_buf(),
        cherries: parsed.cherries,
    })
}

#[derive(Debug)]
struct Parsed {
    id: Option<[u8; 16]>,
    cherries: Vec<Cherry>,
}

/// 解析文件内容。行号从 1 数，出错时报给人看的就是这个号。
///
/// 规矩只有三条：`#` 开头和空行忽略；`id <32 位十六进制>` 是凭证（最多一行）；其余每行
/// 是一条路径（`~/` 开头或绝对路径），篮子里的名字取自最后一段。
fn parse(text: &str) -> Result<Parsed> {
    let mut id = None;
    let mut cherries: Vec<Cherry> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, tail) = match line.split_once(char::is_whitespace) {
            Some((h, t)) => (h, t.trim()),
            None => (line, ""),
        };
        if head == "id" {
            let bytes = hex::decode(tail)
                .ok()
                .filter(|b| b.len() == 16)
                .with_context(|| format!("第 {n} 行：id 后面得是 32 位十六进制"))?;
            if id.is_some() {
                bail!("第 {n} 行：id 只能有一行");
            }
            id = Some(bytes.try_into().expect("上面查过长度"));
            continue;
        }

        let source = expand(line, n)?;
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .with_context(|| format!("第 {n} 行：`{line}` 没有能当名字的最后一段"))?
            .to_owned();
        if cherries.iter().any(|c| c.name == name) {
            bail!("第 {n} 行：篮子里已经有一个叫 `{name}` 的了（两个路径的最后一段撞了）");
        }
        cherries.push(Cherry { name, source });
    }

    Ok(Parsed { id, cherries })
}

/// `~/` 展开成家目录，其余必须是绝对路径。
///
/// 相对路径在这儿没有意义：服务是 launchd 起来的，cwd 是 `/`，报出来的路径只会是误会。
fn expand(raw: &str, n: usize) -> Result<PathBuf> {
    let home = || dirs::home_dir().context("找不到家目录");
    if raw == "~" {
        return home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        bail!("第 {n} 行：`{raw}` 不是绝对路径（可以写成 ~/…）");
    }
    Ok(path)
}

fn mint_id() -> Result<[u8; 16]> {
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).map_err(|e| anyhow::anyhow!("取随机数失败: {e}"))?;
    Ok(id)
}

/// 把 `id` 那一行插进文件，返回插过的内容（测试用）。
///
/// 插在开头那段注释和空行**之后**：手写的说明留在文件头上，`id` 紧贴着真正的配置。
/// 写临时文件再 rename：写到一半断电不会给你留下半个篮子。
fn write_id(path: &Path, id: &[u8; 16]) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let out = with_id(&text, id);

    let tmp = path.with_extension("basket.tmp");
    std::fs::write(&tmp, &out)?;
    // 权限跟着原文件走：id 是凭证，别让它比原来更宽松。
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn with_id(text: &str, id: &[u8; 16]) -> String {
    let line = format!("id {}\n", hex::encode(id));
    let mut at = text.len();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let t = raw.trim();
        if !t.is_empty() && !t.starts_with('#') {
            at = offset;
            break;
        }
        offset += raw.len();
    }

    let mut out = String::with_capacity(text.len() + line.len() + 1);
    out.push_str(&text[..at]);
    // 整份文件都是注释、而且最后一行没换行的时候，别把新行粘在它后面。
    if at > 0 && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&line);
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        dirs::home_dir().unwrap()
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cherry-basket-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn 注释空行和_id_都能认() {
        let text = "\
# 给老王看照片
id 3f2ae0c1d9b84f5a8c1d2e3f4a5b6c7d

~/Pictures
/tmp
";
        let p = parse(text).unwrap();
        assert_eq!(p.id.unwrap()[0], 0x3f);
        assert_eq!(p.cherries.len(), 2);
        assert_eq!(p.cherries[0].name, "Pictures");
        assert_eq!(p.cherries[0].source, home().join("Pictures"));
        assert_eq!(p.cherries[1].name, "tmp");
    }

    #[test]
    fn 相对路径报错并带上行号() {
        let err = parse("# x\n\n./docs\n").unwrap_err().to_string();
        assert!(err.contains("第 3 行"), "{err}");
        assert!(err.contains("不是绝对路径"), "{err}");
    }

    #[test]
    fn 两个路径最后一段撞了要报出来() {
        let err = parse("/a/x.txt\n/b/x.txt\n").unwrap_err().to_string();
        assert!(err.contains("第 2 行") && err.contains("`x.txt`"), "{err}");
    }

    #[test]
    fn id_写坏了两处都要报() {
        assert!(parse("id zz\n").is_err());
        assert!(parse("id 0011223344556677\nid 8899aabbccddeeff\n").is_err());
    }

    /// 文件名里带啥都收，名字原样保留——它不直接进命令（挂载点那段另算，见 link.rs）。
    #[test]
    fn 名字可以带空格和引号() {
        let p = parse("/a/我的 照片 'x'\n").unwrap();
        assert_eq!(p.cherries[0].name, "我的 照片 'x'");
    }

    #[test]
    fn 没有_id_就发一个并插在注释之后() {
        let dir = tempdir();
        let file = dir.join("t.basket");
        std::fs::write(&file, "# 说明\n~/Pictures\n").unwrap();

        let b = load(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# 说明", "手写的说明还在最上面");
        assert_eq!(lines[1], format!("id {}", hex::encode(b.id)));
        assert_eq!(lines[2], "~/Pictures");

        // 再读一次得是同一个 id（已经落在文件里了）。
        assert_eq!(load(&file).unwrap().id, b.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 整份文件只有注释、并且最后没有换行：新行不能粘到注释行后面去。
    #[test]
    fn 全是注释的文件也插得进去() {
        let id = [7u8; 16];
        assert_eq!(
            with_id("# 只有一行", &id),
            format!("# 只有一行\nid {}\n", hex::encode(id))
        );
        assert_eq!(with_id("", &id), format!("id {}\n", hex::encode(id)));
        assert_eq!(
            with_id("~/a\n", &id),
            format!("id {}\n~/a\n", hex::encode(id))
        );
    }

    #[test]
    fn 写不回去就算读不进来() {
        let dir = tempdir();
        let file = dir.join("t.basket");
        std::fs::write(&file, "~/x\n").unwrap();
        let mut perm = std::fs::metadata(&file).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o444);
        std::fs::set_permissions(&file, perm).unwrap();

        // root 无视权限位，那种环境下这条测不了。
        if std::fs::write(dir.join("probe"), b"x").is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let err = load(&file).unwrap_err().to_string();
        assert!(err.contains("写不回去"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
