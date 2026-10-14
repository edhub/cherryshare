//! 创建者侧：应答 GetListing / GetFile。

use iroh::{endpoint::Connection, protocol::ProtocolHandler};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    listing,
    proto::{read_frame, write_frame, WireRequest, WireResponse},
    service::Service,
};

/// 一个连接上可以开多条 bi 流，每条流一个请求。查看者连续点几个文件不用重连。
#[derive(Debug, Clone)]
pub struct Provider {
    svc: Service,
}

impl Provider {
    pub fn new(svc: Service) -> Self {
        Self { svc }
    }

    async fn serve_stream<R, W>(&self, recv: &mut R, send: &mut W) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Some(req) = read_frame::<_, WireRequest>(recv).await? else {
            return Ok(());
        };
        // 内部错误不掀掉连接，回一个 Error 帧就够——「不允许静默失败」。
        let resp = match self.handle(req).await {
            Ok(r) => r,
            Err(e) => WireResponse::Error {
                message: format!("{e:#}"),
            },
        };
        write_frame(send, &resp).await
    }

    async fn handle(&self, req: WireRequest) -> anyhow::Result<WireResponse> {
        match req {
            WireRequest::GetListing { basket_id } => {
                let basket = self.svc.basket_by_id(&basket_id).await?;
                Ok(WireResponse::Listing {
                    entries: listing::scan(&basket)?,
                })
            }
            WireRequest::GetFile { basket_id, path } => {
                let basket = self.svc.basket_by_id(&basket_id).await?;
                let file = listing::resolve(&basket, &path)?;
                if std::fs::metadata(&file)?.is_dir() {
                    // 目录是响应不是制品：让查看者去 listing 里找具体文件。
                    anyhow::bail!("`{}` 是目录，目录不是制品", file.display());
                }
                let (hash, size) = self.svc.hash_file(&file).await?;
                Ok(WireResponse::File { hash, size })
            }
        }
    }
}

impl ProtocolHandler for Provider {
    async fn accept(
        &self,
        connection: Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        // 连接关掉时 accept_bi 出错，就是循环该结束的时候。
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            if self.serve_stream(&mut recv, &mut send).await.is_err() {
                break;
            }
            if let Err(e) = send.finish() {
                tracing::debug!(error = %e, "关流失败");
            }
        }
        Ok(())
    }
}
