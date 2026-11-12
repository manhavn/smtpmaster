//! SMTP router: relay mail through MongoDB-managed SMTP accounts.

pub mod admin;
pub mod cli;
pub mod config;
pub mod duration;
pub mod error;
pub mod maintenance;
pub mod model;
pub mod render;
pub mod route;
pub mod router;
pub mod schedule;
pub mod serde_bson;
pub mod service;
pub mod smtp;
pub mod store;
