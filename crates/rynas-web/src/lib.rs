//! rynas web server: HTTP API (`/api/v1`), live metrics and the web interface.

pub mod api;
pub mod app;
pub mod auth;
pub mod conn;
pub mod error;
pub mod events;
pub mod files;
pub mod hardware;
pub mod listen;
pub mod metrics;
pub mod notify;
pub mod pc_agent;
pub mod proxy;
pub mod rynasd_client;
pub mod static_files;
pub mod storage;
pub mod storage_api;
pub mod streams;
pub mod system;
pub mod tls;
pub mod ups;
