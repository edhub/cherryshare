//! CherryShare 的语义核心。
//!
//! 所有「什么算合法」的判断都只在这一层存在一份：路径规范化、名字唯一、冲突、
//! 状态机。客户端（CLI）只收集输入、呈现输出，不重复这些判断。见 ADR-0003。

pub mod basket;
pub mod config;
pub mod link;
pub mod listing;
pub mod service;

pub use basket::{Basket, BasketId, Cherry, Node};
pub use link::BasketLink;
pub use listing::{CherryKind, Entry};
pub use service::Service;
