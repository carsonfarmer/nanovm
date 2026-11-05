//! nanovm: a dense host for untrusted wasip3 HTTP components, run as durable
//! objects (one live instance per object id, one request at a time) with
//! per-object memory, CPU, disk and egress limits.

pub mod app;
pub mod config;
pub mod egress;
pub mod engine;
pub mod jail;
pub mod journal;
pub mod link;
pub mod metrics;
pub mod object;
pub mod proxy;
#[cfg(feature = "page-fs")]
mod capped;
#[cfg(feature = "page-fs")]
mod page_store;
#[cfg(feature = "page-fs")]
#[path = "page_fs.rs"]
pub mod quota;
#[cfg(not(feature = "page-fs"))]
pub mod quota;
pub mod registry;
pub mod server;
pub mod supervisor;
pub mod table;
pub mod telemetry;
pub mod ws;
