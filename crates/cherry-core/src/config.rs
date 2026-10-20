//! 数据目录里的 `config.toml`（ADR-0010）。
//!
//! **只有一项。** 加字段之前先问「有没有真实场景」：能想到的端口、监听网卡、NFS 开关
//! 这些看着该配，其实要么会把链接格式弄成每台机器一个样，要么会把对称的两个入口弄崩。
//! 不许因为「反正能配」就往里加。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 文件名。跟 `.basket` 住在同一个目录：`is_basket_file` 只认 `.basket`，两边不打架。
pub const FILE: &str = "config.toml";

/// 手写的那份配置。字段少是刻意的，不是还没加完。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 分享者名字，对方打开页面时看到的就是它（「Edward Gu 分享给你的篮子」）。
    ///
    /// 这一项值得配，是因为它解决「对方凭什么相信这页面」：链接落到别人手里时，
    /// 页面上那个产品名提供不了任何信息，一个对得上的名字能让他立刻警觉。
    /// 不写就用这台机器的主机名。
    pub name: Option<String>,
}

impl Config {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(FILE)
    }

    /// 读一份。文件不在就是默认配置——新装的人不该先被逼着写一个文件。
    ///
    /// `deny_unknown_fields` 是这里的重点：手写拼错一个字段名，宁可当场报错，
    /// 也不要静默走默认值，让人对着页面纳闷为什么改了没反应。
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("读不了 {}", path.display())),
        };
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("{} 解析不了", path.display()))?;
        // 空字符串和只剩空白等于没写：`name = ""` 不该显示成一句空话。
        cfg.name = cfg
            .name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cherry-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn 没有文件就是默认配置() {
        let dir = tempdir("missing");
        let cfg = Config::load(&Config::path(&dir)).unwrap();
        assert_eq!(cfg.name, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 一行名字就够() {
        let dir = tempdir("name");
        std::fs::write(
            Config::path(&dir),
            "# 给对方看的名字\nname = \"  Edward Gu  \"\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&Config::path(&dir)).unwrap().name.as_deref(),
            Some("Edward Gu")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 空名字等于没写() {
        let dir = tempdir("empty");
        std::fs::write(Config::path(&dir), "name = \"   \"\n").unwrap();
        assert_eq!(Config::load(&Config::path(&dir)).unwrap().name, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 拼错字段名必须当场报错：静默走默认值是最难查的一类错。
    #[test]
    fn 不认识的字段直接报错() {
        let dir = tempdir("typo");
        std::fs::write(Config::path(&dir), "nmae = \"Edward Gu\"\n").unwrap();
        let err = Config::load(&Config::path(&dir)).unwrap_err();
        assert!(format!("{err:#}").contains("nmae"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
