//! Shared helpers for the switchboard integration tests.
//!
//! Each file under `tests/` is compiled as its own crate, so code shared
//! between them lives here and is pulled in with `mod common;`. Placing it at
//! `common/mod.rs` (rather than `common.rs`) keeps cargo from treating it as a
//! standalone test binary.

// Each test crate uses only the subset of helpers it needs; the rest look dead
// from that crate's point of view.
#![allow(dead_code)]

use treadmill_switchboard::config::{
    DatabaseConfig, GitHubOAuthConfig, LogConfig, MockOAuthConfig, OAuthConfig, ServerConfig,
    ServiceConfig, SwitchboardConfig,
};

/// A throwaway [`SwitchboardConfig`] for tests.
///
/// `gh_uri` points the GitHub token/API endpoints at a local mock (wiremock);
/// the database and the rest of the config are inert placeholders. The auth URL
/// is never contacted — tests drive the callback directly rather than visiting
/// the consent screen.
pub fn test_config(gh_uri: &str) -> SwitchboardConfig {
    SwitchboardConfig {
        database: DatabaseConfig {
            host: "unused".to_string(),
            port: None,
            database: "unused".to_string(),
            user: "unused".to_string(),
            auth: None,
        },
        server: ServerConfig {
            bind_address: "127.0.0.1:0".parse().unwrap(),
            testing_only_tls_config: None,
            trusted_proxy_headers: Vec::new(),
        },
        service: ServiceConfig {
            default_token_timeout: chrono::Duration::hours(1),
            default_job_timeout: chrono::Duration::hours(1),
            default_queue_timeout: chrono::Duration::hours(1),
            match_interval: chrono::Duration::seconds(1),
            host_liveness_timeout: chrono::Duration::seconds(30),
            supervisor_ping_interval: std::time::Duration::from_secs(30),
            supervisor_pong_dead: std::time::Duration::from_secs(60),
            supervisor_reconcile_interval: std::time::Duration::from_secs(30),
        },
        log: LogConfig {
            use_tokio_console_subscriber: false,
        },
        oauth: OAuthConfig {
            github: Some(GitHubOAuthConfig {
                client_id: "test-client".to_string(),
                client_secret: "test-secret".to_string(),
                redirect_url: "http://localhost/api/v1/auth/github/callback".to_string(),
                auth_url: "http://localhost/login/oauth/authorize".to_string(),
                token_url: format!("{gh_uri}/login/oauth/access_token"),
                api_base_url: gh_uri.to_string(),
                scopes: vec![
                    "read:user".to_string(),
                    "user:email".to_string(),
                    "read:org".to_string(),
                ],
            }),
            mock: None,
            browser_success_redirect: None,
        },
    }
}

/// A [`SwitchboardConfig`] with only the development-only mock provider enabled
/// (no GitHub). Used by the mock-login test, which needs no external server.
pub fn test_config_mock() -> SwitchboardConfig {
    let mut cfg = test_config("http://unused.invalid");
    cfg.oauth.github = None;
    cfg.oauth.mock = Some(MockOAuthConfig { enabled: true });
    cfg
}
