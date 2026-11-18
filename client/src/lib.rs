//! Client-side mechanics for the Treadmill distributed hardware testbed.
//!
//! [`treadmill_rs`] defines the wire types and, behind its `client` feature, a
//! typed HTTP client for the switchboard API. This crate is the layer above
//! it: where the token comes from, where it is stored, how a login is driven to
//! completion, how a per-job service token is minted and cached, and how the
//! WebSocket bridge to a job service is opened.
//!
//! `tml` (the `treadmill-cli` crate) is built on it, and is the reference for
//! how the pieces fit together.
//!
//! Nothing here writes to a terminal. What a human might want to see is
//! reported through a [`diagnostics::Diagnostics`] the caller supplies, and
//! [`diagnostics::Silent`] discards it.
//!
//! ```no_run
//! use std::sync::Arc;
//! use treadmill_client::diagnostics::Silent;
//! use treadmill_client::session::{Options, Session};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // The same profile `tml` would use, and the session `tml login` stored.
//! let session = Session::load(&Options::default(), Arc::new(Silent))?;
//! let client = session.authenticated_client()?;
//! println!("{}", client.whoami().await?.name);
//! # Ok(())
//! # }
//! ```
//!
//! `examples/run-job.rs` goes the whole way: enqueue a job, wait for it, and
//! open a local port onto its SSH service.

#[cfg(feature = "profile")]
pub mod config;
#[cfg(feature = "dbus")]
pub mod daemon;
pub mod diagnostics;
pub mod error;
pub mod job;
pub mod locked_file;
#[cfg(feature = "profile")]
pub mod login;
#[cfg(feature = "profile")]
pub mod service;
#[cfg(feature = "profile")]
pub mod session;
pub mod ssh;
pub mod state;
#[cfg(feature = "ws")]
pub mod wsproxy;
