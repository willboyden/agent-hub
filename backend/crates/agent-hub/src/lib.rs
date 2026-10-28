//! Agent Hub (Rust port): the server (`agent-hub`), the operator CLI (`hubctl`) and the client adapters.
//! Contract and module ownership: backend/PORTING.md.

pub mod adapters;
pub mod api;
pub mod cli;
pub mod config;
pub mod desktop;
pub mod server;
pub mod services;
pub mod telemetry;
