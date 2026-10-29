//! nanovm: a dense host for untrusted wasip3 HTTP components, run as durable
//! objects (one live instance per object id, one request at a time) with
//! per-object memory, CPU, disk and egress limits.

pub mod app;
pub mod config;
pub mod egress;
pub mod engine;
pub mod jail;
pub mod kv;
pub mod link;
pub mod metrics;
pub mod object;
pub mod quota;
pub mod registry;
pub mod server;
pub mod supervisor;
pub mod table;
pub mod ws;
