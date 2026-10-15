//! basket 与 cherry。
//!
//! basket 是一篮 cherry，不是一个目录：文件留在原处，这里只记录引用。
//! 见 ADR-0001。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CherryKind {
    File,
    Dir,
}

/// 篮子里的东西。放进篮子的只是引用，源文件不动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cherry {
    /// 在篮子里的名字，也是 listing 路径的第一段。
    pub name: String,
    /// 创建者磁盘上的绝对路径。
    pub source: PathBuf,
    pub kind: CherryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Basket {
    /// 128 bit 随机（ADR-0002）。需要的是不可猜测性，不是唯一性。
    pub id: [u8; 16],
    pub name: String,
    pub cherries: Vec<Cherry>,
}

impl Basket {
    pub fn new(name: &str) -> Result<Self> {
        validate_name(name).context("basket 名不合法")?;
        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id).map_err(|e| anyhow::anyhow!("取随机数失败: {e}"))?;
        Ok(Self {
            id,
            name: name.to_owned(),
            cherries: Vec::new(),
        })
    }

    pub fn cherry(&self, name: &str) -> Option<&Cherry> {
        self.cherries.iter().find(|c| c.name == name)
    }

    /// 放一颗 cherry 进篮子。名字在篮子内唯一；改名只改展示路径，不动源文件。
    pub fn add(&mut self, source: &Path) -> Result<Cherry> {
        // 相对路径在这儿解析出来的是「相对于 /」——服务跑在 launchd 底下，cwd 就是 /。
        // 那多半不是用户想的东西，宁可报错。客户端应该已经把它变成绝对路径了。
        if !source.is_absolute() {
            bail!(
                "`{}` 是相对路径，只有客户端知道它指哪儿；篮子里记的必须是绝对路径",
                source.display()
            );
        }
        let source = source
            .canonicalize()
            .with_context(|| format!("找不到 {}", source.display()))?;
        let meta = std::fs::symlink_metadata(&source)
            .with_context(|| format!("读不到 {}", source.display()))?;
        // 「只有真文件和真目录能对外」只有一处判断（`listing::servable`），进篮子的时候
        // 也得问它：不然会放进来一个 listing 永远不列、也永远挂不出来的东西。
        let kind = crate::listing::servable(&meta)
            .with_context(|| format!("{} 不是普通文件也不是真目录，不共享", source.display()))?;

        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("{} 没有可用的文件名", source.display()))?
            .to_owned();
        validate_name(&name).context("cherry 名不合法")?;
        if self.cherry(&name).is_some() {
            bail!("篮子里已经有一颗叫 `{name}` 的 cherry 了");
        }

        let cherry = Cherry { name, source, kind };
        self.cherries.push(cherry.clone());
        Ok(cherry)
    }

    pub fn remove(&mut self, name: &str) -> Result<Cherry> {
        let idx = self
            .cherries
            .iter()
            .position(|c| c.name == name)
            .with_context(|| format!("篮子里没有叫 `{name}` 的 cherry"))?;
        Ok(self.cherries.remove(idx))
    }
}

/// 名字会变成 listing 里的路径段，所以不能含分隔符、不能是 `.` / `..`。
/// 名字：不能带路径分隔符。
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("名字不能为空");
    }
    if name == "." || name == ".." {
        bail!("`{name}` 不是合法名字");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("名字里不能有路径分隔符");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 名字在同一篮子内唯一() {
        let dir = tempdir();
        let f = dir.join("a.txt");
        std::fs::write(&f, b"hi").unwrap();

        let mut b = Basket::new("test").unwrap();
        b.add(&f).unwrap();
        // 同一个源再加一次会被名字挡住。
        let err = b.add(&f).unwrap_err().to_string();
        assert!(err.contains("已经有一颗"), "{err}");
    }

    /// 「只有真文件和真目录能对外」这条规则在进篮子的时候就该生效：放进来一个 listing
    /// 永远不列的东西，等于静默地分享了一个不存在的东西。
    #[test]
    fn 拒绝字符设备之类() {
        let mut b = Basket::new("test").unwrap();
        let err = b.add(Path::new("/dev/null")).unwrap_err().to_string();
        assert!(err.contains("不共享"), "{err}");
    }

    #[test]
    fn 拒绝相对路径() {
        // 服务这头的 cwd 是 /，相对路径在这儿只会解析成怪东西。
        let mut b = Basket::new("test").unwrap();
        let err = b.add(Path::new("docs")).unwrap_err().to_string();
        assert!(err.contains("相对路径"), "{err}");
    }

    #[test]
    fn 拒绝越界的名字() {
        assert!(Basket::new("a/b").is_err());
        assert!(Basket::new("..").is_err());
        assert!(Basket::new("").is_err());
    }

    /// cherry 名来自磁盘上的文件名——`my file.txt` 这种必须能进篮子，而它只出现在
    #[test]
    fn id_是随机的() {
        let a = Basket::new("x").unwrap();
        let b = Basket::new("x").unwrap();
        assert_ne!(a.id, b.id);
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cherry-test-{}",
            hex::encode(Basket::new("t").unwrap().id)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
