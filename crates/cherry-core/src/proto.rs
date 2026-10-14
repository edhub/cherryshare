//! 自研 ALPN 上的两个请求。见 ADR-0002。

use anyhow::{Context, Result, bail};
use iroh_blobs::Hash;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::listing::Entry;

pub const ALPN: &[u8] = b"cherryshare/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireRequest {
    /// 廉价：遍历 + stat，不算 hash。
    GetListing { basket_id: [u8; 16] },
    /// 昂贵：懒 rehash，耗时只与被点的那一个文件的大小成正比。
    GetFile { basket_id: [u8; 16], path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireResponse {
    Listing { entries: Vec<Entry> },
    /// 只回 hash；字节让查看者走 iroh-blobs 的原生 ALPN 自己取，我们不搬文件。
    File { hash: Hash, size: u64 },
    Error { message: String },
}

/// 一帧最大 16 MiB。listing 可以很大，但不该无限大。
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// 长度前缀 + postcard。
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = postcard::to_stdvec(msg).context("序列化失败")?;
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// 对端干净关闭时返回 `None`。
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = match r.read_u32().await {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("读帧长度失败"),
    };
    if len > MAX_FRAME {
        bail!("帧太大：{len} 字节");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await.context("读帧内容失败")?;
    Ok(Some(postcard::from_bytes(&buf).context("反序列化失败")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 帧往返() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &WireRequest::GetFile {
            basket_id: [1u8; 16],
            path: "a/b.txt".into(),
        })
        .await
        .unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let got: WireRequest = read_frame(&mut cur).await.unwrap().unwrap();
        matches!(got, WireRequest::GetFile { .. });
    }

    #[tokio::test]
    async fn 读到结尾返回_none() {
        let mut cur = std::io::Cursor::new(Vec::new());
        let got: Option<WireRequest> = read_frame(&mut cur).await.unwrap();
        assert!(got.is_none());
    }
}
