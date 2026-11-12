//! Minimal SMTP implementation: just enough protocol for a relay.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::send;
