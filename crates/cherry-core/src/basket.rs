//! basket 与 cherry。
//!
//! 一个篮子就是数据目录里的一个 `<名字>.basket` 文件（ADR-0008）：一行一条要分享的路径，
//! `id` 一行是访问凭证。**文件是唯一真相**——服务只读它，唯一一处写入是补一个缺失的 `id`。
//!
//! 篮子不是一个目录：文件留在原处，这里只记录引用。见 ADR-0001。
//!
//! 一行路径中间可以有 `/./`：它前面那段只用来拼源路径，后面那段是篮子里的可见路径
//! （ADR-0009）。可见路径可能不止一段，所以篮子根长出来的是一棵树：中间层是虚拟目录，
//! 只有被点名的叶子对到磁盘。

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};

/// 篮子文件的后缀。数据目录里其它的东西（编辑器备份、`.migrated`）一概不认。
pub const SUFFIX: &str = ".basket";

/// 篮子的 id：文件里 `id` 那一行、链接里那一段、NFS 挂载路径的第一段，都是它。
///
/// 默认是 128 bit 随机（[`BasketId::mint`]）；手写的也收，因为想记住一个篮子的凭证是
/// 正当需求。代价落在写的人身上：它是**唯一的锁**（ADR-0002）——链接里除了主机地址
/// 就只有它。
///
/// 字符集卡在 `[A-Za-z0-9._-]`：这段东西会进网页 URL 的路径、进对方要粘的那行 shell
/// 命令、也进 NFS 的导出路径。空白、斜杠、冒号、引号、`%` 都会把其中一处搞坏。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BasketId(String);

impl BasketId {
    /// 手写 id 的长度上限。够长了，而且让报错的话说得清楚。
    pub const MAX_LEN: usize = 128;

    /// 校验一个手写的 id。
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            bail!("id 后面得跟点什么");
        }
        if s.len() > Self::MAX_LEN {
            bail!("id 太长（最多 {} 个字符）", Self::MAX_LEN);
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("`{s}` 里有当不了 id 的字符（只给字母数字和 `-` `_` `.` 留位置）");
        }
        Ok(Self(s.to_owned()))
    }

    /// 128 bit 随机，32 位十六进制。文件里没有 `id` 行时发这个。
    pub fn mint() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("取随机数失败: {e}"))?;
        Ok(Self::from_bytes(bytes))
    }

    /// 16 个字节写成 32 位十六进制。迁移老数据用。
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(hex::encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 挂载点里那一段：id 的前 8 个字符，短了就全给。
    ///
    /// 字符集是 ASCII，所以按字节切不会切坏。
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(8)]
    }
}

impl std::fmt::Display for BasketId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for BasketId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// 篮子里的东西。放进来的是引用，源文件不动。
#[derive(Debug, Clone)]
pub struct Cherry {
    /// 在篮子里的可见路径，可能带 `/`（`/./` 后面那段）。
    pub path: String,
    /// 创建者磁盘上的绝对路径。
    pub source: PathBuf,
    /// 文件里的行号。只用来报撞名/包含关系时指哪一行。
    pub line: usize,
}

/// 可见路径的树节点。`source` 有值表示这里正好是一颗 cherry；否则是虚拟目录。
#[derive(Debug, Default)]
pub struct Node {
    pub source: Option<PathBuf>,
    pub children: BTreeMap<String, Node>,
    /// 这颗 cherry，或者这棵子树里第一条的行号。报错用。
    line: usize,
    /// 这颗 cherry 的可见路径（虚拟节点没有）。报错用。
    visible: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Basket {
    /// 访问凭证（ADR-0002）。文件里手写，没写才自动发一个。
    pub id: BasketId,
    /// 文件名去掉 `.basket`。改了文件名就是改了篮子名，id 不变。
    pub name: String,
    /// 定义这个篮子的文件（`<数据目录>/<名字>.basket`）。
    pub file: PathBuf,
    pub cherries: Vec<Cherry>,
    /// 可见路径的树。只由 `.basket` 文件决定，源文件变化不用重建；克隆只加引用计数。
    tree: Arc<Node>,
    /// 定义这个篮子的文件的 mtime（Unix 秒）。虚拟目录拿它当 mtime。
    file_mtime: u64,
}

impl Basket {
    /// 用一组 cherry 造一个篮子。主要给测试和迁移用；正常读文件走 [`load`]。
    pub fn new(id: BasketId, name: String, file: PathBuf, cherries: Vec<Cherry>) -> Result<Self> {
        let tree = build_tree(&cherries)?;
        Ok(Self::with_tree(id, name, file, cherries, tree))
    }

    fn with_tree(
        id: BasketId,
        name: String,
        file: PathBuf,
        cherries: Vec<Cherry>,
        tree: Node,
    ) -> Self {
        let file_mtime = std::fs::metadata(&file)
            .map(|m| crate::listing::mtime_of(&m))
            .unwrap_or(0);
        Self {
            id,
            name,
            file,
            cherries,
            tree: Arc::new(tree),
            file_mtime,
        }
    }

    pub fn tree(&self) -> &Node {
        &self.tree
    }

    /// 只看虚拟树：路径必须落在虚拟目录上（根或中间层）。落在真 cherry 上或它里面返回 None。
    pub fn virtual_node(&self, path: &str) -> Option<&Node> {
        let mut node = &*self.tree;
        if path.is_empty() {
            return Some(node);
        }
        for seg in path.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return None;
            }
            let child = node.children.get(seg)?;
            if child.source.is_some() {
                return None;
            }
            node = child;
        }
        Some(node)
    }

    /// 虚拟目录的 mtime：定义这个篮子的那个文件的时间。
    ///
    /// 篮子结构只由这份文件决定，所以它变了才需要让客户端的目录缓存失效。源文件内容
    /// 变化只影响那一条自己的属性，不影响目录。
    pub fn mtime(&self) -> u64 {
        self.file_mtime
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
            let id = BasketId::mint()?;
            write_id(path, &id)
                .with_context(|| format!("文件里没有 id，也写不回去 {}", path.display()))?;
            tracing::info!(basket = %name, file = %path.display(), "文件里没有 id，发了一个并写回去");
            id
        }
    };

    Ok(Basket::with_tree(
        id,
        name,
        path.to_path_buf(),
        parsed.cherries,
        parsed.tree,
    ))
}

#[derive(Debug)]
struct Parsed {
    id: Option<BasketId>,
    cherries: Vec<Cherry>,
    tree: Node,
}

/// 解析文件内容。行号从 1 数，出错时报给人看的就是这个号。
///
/// 规矩只有四条：`#` 开头和空行忽略；`id <凭证>` 是凭证（最多一行）；其余每行
/// 是一条路径（`~/` 开头或绝对路径）；路径里第一个 `.` 段把「源」和「可见路径」分开。
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
            let parsed = BasketId::parse(tail).with_context(|| format!("第 {n} 行"))?;
            if id.is_some() {
                bail!("第 {n} 行：id 只能有一行");
            }
            id = Some(parsed);
            continue;
        }

        let (path, source) = split_line(line, n)?;
        cherries.push(Cherry {
            path,
            source,
            line: n,
        });
    }

    let tree = build_tree(&cherries)?;
    Ok(Parsed { id, cherries, tree })
}

/// 一行路径 →（篮子里的可见路径，磁盘上的源路径）。
///
/// 第一个 `.` 段是分界（`/./`）：前面只用来拼源路径，后面就是篮子里的层级。没有 `.` 段就
/// 还是老规矩，取最后一段当名字。可见路径可以带空格；名字段不能有空的、`.` 或 `..`。
fn split_line(raw: &str, n: usize) -> Result<(String, PathBuf)> {
    // 结尾的斜杠没有意义，先去掉：`~/a/` 和 `~/a` 该是同一颗。
    let trimmed = raw.trim_end_matches('/');
    let raw = if trimmed.is_empty() { "/" } else { trimmed };
    if !(raw == "~" || raw.starts_with("~/") || raw.starts_with('/')) {
        bail!("第 {n} 行：`{raw}` 不是绝对路径（可以写成 ~/…）");
    }

    let comps: Vec<&str> = raw.split('/').collect();
    let Some(at) = comps.iter().position(|c| *c == ".") else {
        let source = expand(raw, n)?;
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty() && *n != "." && *n != "..")
            .with_context(|| format!("第 {n} 行：`{raw}` 没有能当名字的最后一段"))?
            .to_owned();
        return Ok((name, source));
    };

    let visible_segs = &comps[at + 1..];
    if visible_segs.is_empty() {
        bail!("第 {n} 行：`/./` 后面得有一段可见路径");
    }
    for seg in visible_segs {
        if seg.is_empty() || *seg == "." || *seg == ".." {
            bail!("第 {n} 行：`/./` 后面的可见路径不能有空段、`.` 或 `..`");
        }
    }

    let visible = visible_segs.join("/");
    let prefix = comps[..at].join("/");
    let raw_source = if prefix.is_empty() {
        format!("/{visible}")
    } else {
        format!("{prefix}/{visible}")
    };
    let source = expand(&raw_source, n)?;
    Ok((visible, source))
}

/// 把可见路径插进树。撞名和「一条盖住另一条」都在这里挡下来，报两边的行号。
fn build_tree(cherries: &[Cherry]) -> Result<Node> {
    let mut root = Node::default();
    for c in cherries {
        let segs: Vec<&str> = c.path.split('/').collect();
        insert(&mut root, &segs, c)?;
    }
    Ok(root)
}

fn insert(node: &mut Node, segs: &[&str], c: &Cherry) -> Result<()> {
    if node.source.is_some() {
        let at = node.visible.as_deref().unwrap_or("?");
        if segs.is_empty() {
            bail!(
                "第 {} 行：`{}` 和前面第 {} 行的 `{}` 重了",
                c.line,
                c.path,
                node.line,
                at
            );
        }
        bail!(
            "第 {} 行：`{}` 落在第 {} 行的 `{}` 里面；一颗 cherry 和它里面的东西不能同时点名",
            c.line,
            c.path,
            node.line,
            at
        );
    }

    let Some((head, rest)) = segs.split_first() else {
        if let Some(child) = node.children.values().next() {
            let leaf = first_leaf(child);
            let at = leaf.and_then(|n| n.visible.as_deref()).unwrap_or("?");
            let line = leaf.map(|n| n.line).unwrap_or(child.line);
            bail!(
                "第 {} 行：`{}` 盖住了第 {} 行的 `{}`；一颗 cherry 和它里面的东西不能同时点名",
                c.line,
                c.path,
                line,
                at
            );
        }
        node.source = Some(c.source.clone());
        node.visible = Some(c.path.clone());
        node.line = c.line;
        return Ok(());
    };

    let child = node
        .children
        .entry((*head).to_owned())
        .or_insert_with(|| Node {
            line: c.line,
            ..Node::default()
        });
    insert(child, rest, c)
}

fn first_leaf(node: &Node) -> Option<&Node> {
    if node.source.is_some() {
        return Some(node);
    }
    node.children.values().find_map(first_leaf)
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

/// 把 `id` 那一行插进文件，返回插过的内容（测试用）。
///
/// 插在开头那段注释和空行**之后**：手写的说明留在文件头上，`id` 紧贴着真正的配置。
/// 写临时文件再 rename：写到一半断电不会给你留下半个篮子。
fn write_id(path: &Path, id: &BasketId) -> Result<()> {
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

fn with_id(text: &str, id: &BasketId) -> String {
    let line = format!("id {id}\n");
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
        assert_eq!(p.id.unwrap().as_str(), "3f2ae0c1d9b84f5a8c1d2e3f4a5b6c7d");
        assert_eq!(p.cherries.len(), 2);
        assert_eq!(p.cherries[0].path, "Pictures");
        assert_eq!(p.cherries[0].source, home().join("Pictures"));
        assert_eq!(p.cherries[1].path, "tmp");
    }

    #[test]
    fn 相对路径报错并带上行号() {
        let err = parse("# x\n\n./docs\n").unwrap_err().to_string();
        assert!(err.contains("第 3 行"), "{err}");
        assert!(err.contains("不是绝对路径"), "{err}");
    }

    #[test]
    fn 两条路径最后一段撞了要报出来() {
        let err = parse("/a/x.txt\n/b/x.txt\n").unwrap_err().to_string();
        assert!(err.contains("第 2 行") && err.contains("`x.txt`"), "{err}");
        assert!(err.contains("第 1 行"), "另一行也得指出来: {err}");
    }

    /// `/./` 前面只用来拼源路径，后面是篮子里的可见路径（ADR-0009）。
    #[test]
    fn 斜杠点保留目录层级() {
        let text = "\
~/.pi/./agent/APPEND_SYSTEM.md
/Users/edh/dev/mt6000/./edproxy/dist
";
        let p = parse(text).unwrap();
        assert_eq!(p.cherries[0].path, "agent/APPEND_SYSTEM.md");
        assert_eq!(
            p.cherries[0].source,
            home().join(".pi/agent/APPEND_SYSTEM.md")
        );
        assert_eq!(p.cherries[1].path, "edproxy/dist");
        assert_eq!(
            p.cherries[1].source,
            PathBuf::from("/Users/edh/dev/mt6000/edproxy/dist")
        );
    }

    /// 两个都叫 `dist` 的目录，只要 `/./` 后面的前缀不同，就不撞。
    #[test]
    fn 不同前缀的同名目录不撞() {
        let p = parse("/a/./edproxy/dist\n/b/./cherryshare/dist\n").unwrap();
        assert_eq!(p.cherries.len(), 2);
        assert_eq!(p.tree.children.len(), 2);
        assert!(p.tree.children["edproxy"].source.is_none());
        assert!(p.tree.children["cherryshare"].source.is_none());
        assert_eq!(
            p.tree.children["edproxy"].children["dist"]
                .source
                .as_deref(),
            Some(Path::new("/a/edproxy/dist"))
        );
    }

    #[test]
    fn 可见路径重了要指到两行() {
        let err = parse("/a/./x\n/b/./x\n").unwrap_err().to_string();
        assert!(err.contains("第 1 行") && err.contains("第 2 行"), "{err}");
    }

    /// 一棵 cherry 和它里面的东西不能同时点名：那会搞不清 `agent` 是整颗还是外壳。
    #[test]
    fn 一颗cherry不能盖住它里面的路径() {
        for text in ["/a/./x\n/b/./x/y\n", "/a/./x/y\n/b/./x\n"] {
            let err = parse(text).unwrap_err().to_string();
            assert!(
                err.contains("第 1 行") && err.contains("第 2 行"),
                "{text}: {err}"
            );
            assert!(err.contains("不能同时点名"), "{text}: {err}");
        }
    }

    #[test]
    fn 斜杠点后面不能是空的或点() {
        for bad in ["~/a/./\n", "~/a/./../b\n", "~/a/.//b\n"] {
            assert!(parse(bad).is_err(), "该拒：{bad:?}");
        }
        // 相对路径开头的 `.` 不是分界，仍然按相对路径拒。
        assert!(parse("./a/./b\n").is_err());
    }

    /// 中间层是虚拟目录，叶子才是真 cherry。
    #[test]
    fn 树按可见路径分层() {
        let p = parse("/a/./agent/x\n/a/./agent/y/z\n/a/./agent/skills/s\n").unwrap();
        let agent = &p.tree.children["agent"];
        assert!(agent.source.is_none());
        assert_eq!(agent.children.len(), 3);
        assert!(agent.children["x"].source.is_some());
        assert!(agent.children["y"].source.is_none());
        assert!(agent.children["y"].children["z"].source.is_some());
    }

    /// 手写的 id 原样就是凭证，不用是十六进制。
    #[test]
    fn 手写一个简单_id_也认() {
        let p = parse("id photos\n/a\n").unwrap();
        assert_eq!(p.id.unwrap().as_str(), "photos");
    }

    #[test]
    fn id_写坏了两处都要报() {
        assert!(parse("id\n").is_err(), "后面什么都没有");
        assert!(parse("id a/b\n").is_err(), "斜杠进不了 id");
        assert!(parse("id 0011223344556677\nid 8899aabbccddeeff\n").is_err());
    }

    /// 文件名里带啥都收，名字原样保留——它不直接进命令（挂载点那段另算，见 link.rs）。
    #[test]
    fn 名字可以带空格和引号() {
        let p = parse("/a/我的 照片 'x'\n").unwrap();
        assert_eq!(p.cherries[0].path, "我的 照片 'x'");
    }

    /// `/./` 后面的可见路径也可以带空格；路径和名字之间没有第二层分隔符。
    #[test]
    fn 可见路径可以带空格() {
        let p = parse("/a/./我的 照片/x\n").unwrap();
        assert_eq!(p.cherries[0].path, "我的 照片/x");
        assert_eq!(p.cherries[0].source, PathBuf::from("/a/我的 照片/x"));
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
        assert_eq!(lines[1], format!("id {}", b.id));
        assert_eq!(b.id.as_str().len(), 32, "发出来的还是 32 位十六进制");
        assert_eq!(lines[2], "~/Pictures");

        // 再读一次得是同一个 id（已经落在文件里了）。
        assert_eq!(load(&file).unwrap().id, b.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 整份文件只有注释、并且最后没有换行：新行不能粘到注释行后面去。
    #[test]
    fn 全是注释的文件也插得进去() {
        let id = BasketId::from_bytes([7u8; 16]);
        assert_eq!(with_id("# 只有一行", &id), format!("# 只有一行\nid {id}\n"));
        assert_eq!(with_id("", &id), format!("id {id}\n"));
        assert_eq!(with_id("~/a\n", &id), format!("id {id}\n~/a\n"));
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
