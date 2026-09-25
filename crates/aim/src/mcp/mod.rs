//! Model Context Protocol edges: trusted user-server clients and the local service façade.

mod bridge;
mod cache;
pub mod client;
pub mod config;
pub mod proxy;
pub mod server;
pub mod services;
pub mod session;
pub mod trust;

pub use bridge::WorkspacePipe;
pub use server::AimMcpServer;
