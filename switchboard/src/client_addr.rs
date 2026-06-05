//! Resolving the real client address of a request.
//!
//! Behind a reverse proxy the raw TCP peer is the proxy, not the user, so the
//! true client address must be read from a proxy-set header (e.g.
//! `X-Forwarded-For`). Trusting such a header unconditionally would let any
//! client spoof its recorded address, so this is gated on
//! [`ServerConfig::trusted_proxy_headers`](crate::config::ServerConfig::trusted_proxy_headers):
//! the headers are consulted ONLY when an operator has explicitly listed them,
//! signalling that a trusted proxy sets them. With no configured headers, the
//! socket peer is the sole source of truth.
//!
//! The resolved address is recorded on issued tokens and stamped into login
//! audit events, so it must be faithful, not guessed.

use std::net::{IpAddr, SocketAddr};

use axum::extract::ConnectInfo;
use http::request::Parts;

use crate::config::ServerConfig;

/// A resolved client address. `port` is only known when the address came from
/// the socket peer; proxy headers such as `X-Forwarded-For` carry an IP with no
/// port, so it is `None` in that case.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr {
    pub ip: IpAddr,
    pub port: Option<u16>,
}

impl ClientAddr {
    /// Resolve the client address for a request from its parts, honouring the
    /// configured trusted-proxy headers.
    ///
    /// Resolution order:
    ///   1. Each configured header, in order; the first that yields a parseable
    ///      IP wins (for a comma-separated list like `X-Forwarded-For`, the
    ///      left-most entry — the original client — is used).
    ///   2. Otherwise the raw socket peer from `ConnectInfo<SocketAddr>`.
    ///
    /// Returns `None` only if neither a trusted header nor connection info is
    /// available (e.g. a synthetic request with no `ConnectInfo`).
    pub fn resolve(parts: &Parts, config: &ServerConfig) -> Option<ClientAddr> {
        for header_name in &config.trusted_proxy_headers {
            let Some(value) = parts.headers.get(header_name.as_str()) else {
                continue;
            };
            let Ok(value) = value.to_str() else {
                continue;
            };
            // `X-Forwarded-For` is `client, proxy1, proxy2, ...`; the original
            // client is the left-most element.
            if let Some(ip) = value
                .split(',')
                .next()
                .map(str::trim)
                .and_then(|s| s.parse::<IpAddr>().ok())
            {
                return Some(ClientAddr { ip, port: None });
            }
        }

        parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| ClientAddr {
                ip: addr.ip(),
                port: Some(addr.port()),
            })
    }

    /// The IP rendered as text for storage (`api_tokens.created_ip`,
    /// audit payloads).
    pub fn ip_string(&self) -> String {
        self.ip.to_string()
    }
}
