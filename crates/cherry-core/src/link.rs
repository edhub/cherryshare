//! BasketLink：分享出去的那条链接。见 ADR-0002。

use std::{fmt, str::FromStr};

use iroh::EndpointAddr;
use iroh_tickets::{ParseError, Ticket};
use serde::{Deserialize, Serialize};

/// 分享的单位就是一个 link。
///
/// 带完整的 [`EndpointAddr`] 而不只是 `EndpointId`：LAN 模式下多带的地址信息
/// 只是冗余，但组播被网络设备挡住、或者两台机器不在同一个网段时它还能兜底。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasketLink {
    pub addr: EndpointAddr,
    /// 凭证，不是索引：谁拿到谁就能看。吊销 = 换一个新的 `basket_id`。
    pub basket_id: [u8; 16],
}

impl BasketLink {
    pub fn new(addr: EndpointAddr, basket_id: [u8; 16]) -> Self {
        Self { addr, basket_id }
    }
}

impl Ticket for BasketLink {
    const KIND: &'static str = "cherryshare";

    fn encode_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("postcard 序列化不会失败")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

impl FromStr for BasketLink {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ticket::decode_string(s)
    }
}

impl fmt::Display for BasketLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", Ticket::encode_string(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{SecretKey, TransportAddr};

    fn sample() -> BasketLink {
        let id = SecretKey::generate().public();
        let addr = EndpointAddr::from_parts(id, [TransportAddr::Ip("192.168.8.189:1234".parse().unwrap())]);
        BasketLink::new(addr, [7u8; 16])
    }

    #[test]
    fn 往返一致() {
        let link = sample();
        let s = link.to_string();
        assert!(s.starts_with("cherryshare"), "前缀不对: {s}");
        assert_eq!(s.parse::<BasketLink>().unwrap(), link);
    }

    #[test]
    fn 换掉前缀会被拒() {
        let mut s = sample().to_string();
        s.replace_range(..11, "blobxxxxxxx");
        assert!(s.parse::<BasketLink>().is_err());
    }
}
