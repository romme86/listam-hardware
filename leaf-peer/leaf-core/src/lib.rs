//! Platform-independent leaf peer: mirrors a set of hypercores over a single
//! duplex stream speaking the hypercore v10/v11 wire protocol.
//!
//! The set of cores to mirror is learned from a "control core" (a hub-written
//! hypercore whose key is provisioned out of band) carrying JSON lines:
//! `{"add": ["<core key hex>", ...]}`.

pub mod mirror;

pub use mirror::*;
