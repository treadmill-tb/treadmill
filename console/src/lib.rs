//! The treadmill web console: a server-side-rendered frontend for the
//! switchboard REST API.
//!
//! The console is a thin, stateless HTTP server. It holds no database and no
//! session store of its own: a logged-in browser carries a switchboard bearer
//! token in an `HttpOnly` cookie, and every page is rendered from data fetched
//! live through the typed [`SwitchboardClient`] over that token. Because every
//! response type is the shared `treadmill_rs::api::switchboard` type, any change
//! to the API surface is a compile error here rather than a runtime surprise.
//!
//! [`SwitchboardClient`]: treadmill_rs::api::switchboard::client::SwitchboardClient

pub mod assets;
pub mod config;
pub mod routes;
pub mod serve;
pub mod session;
pub mod views;
