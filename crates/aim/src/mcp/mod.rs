//! Model Context Protocol edges: trusted user-server clients and the local service façade.

mod bridge;
pub mod client;
pub mod config;
pub mod server;
pub mod services;
pub mod trust;

pub use bridge::WorkspacePipe;
pub use server::AimMcpServer;
