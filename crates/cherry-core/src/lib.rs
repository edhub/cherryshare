//! CherryShare 的语义核心。
//!
//! 所有「什么算合法」的判断都只在这一层存在一份：路径规范化、名字唯一、冲突、
//! 状态机。客户端（GUI / CLI / mount）只收集输入、呈现输出，不重复这些判断。
//! 见 ADR-0003。

pub mod basket;
pub mod endpoint;
pub mod index;
pub mod link;
pub mod listing;
pub mod proto;
pub mod provider;
pub mod service;

pub use basket::{Basket, Cherry, CherryKind};
pub use link::BasketLink;
pub use listing::Entry;
pub use proto::{ALPN, WireRequest, WireResponse};
pub use service::{Fetched, Notice, Service};
