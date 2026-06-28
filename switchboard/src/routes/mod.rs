mod auth;
mod hosts;
mod images;
mod jobs;
mod users;

use crate::config::EmbeddedConsoleConfig;
use crate::serve::AppState;
use aide::axum::ApiRouter;
use aide::axum::routing::{delete_with, get_with, post_with, put_with};
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tower_http::trace::TraceLayer;
use treadmill_console::config::{
    ConsoleConfig, ServerConfig as ConsoleServerConfig,
    SwitchboardConfig as ConsoleSwitchboardConfig,
};
use treadmill_console::serve::AppState as ConsoleAppState;

pub fn build_router(state: AppState) -> Router<()> {
    // Optionally serve the web console at `/`, on this same listener. Built
    // before `state` is moved into the API router below.
    let console = state
        .config()
        .console
        .as_ref()
        .filter(|c| c.enabled)
        .map(|c| embedded_console_router(&state, c));

    let mut router: Router<()> = ApiRouter::new()
        // -- INSERT ROUTES HERE --
        .nest_api_service("/api/v1", api_router().with_state(state.clone()))
        // utility
        .fallback(not_found)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .into();

    // Mount the console at the root, next to `/api/v1`. The API was built with a
    // custom `not_found` fallback, which is preserved across the merge; the
    // console brings only routes (its default 404 yields to ours).
    if let Some(console) = console {
        router = router.merge(console);
    }
    router
}

/// Build the embedded console's router, pointed back at this switchboard.
///
/// The console is an HTTP client of the switchboard API even when embedded;
/// `api_base_url` therefore defaults to a loopback URL for this very process, so
/// the only real difference from running the console separately is that it
/// shares this listener.
fn embedded_console_router(state: &AppState, cfg: &EmbeddedConsoleConfig) -> Router {
    let bind_address = state.config().server.bind_address;
    let loopback = format!("http://127.0.0.1:{}", bind_address.port());

    let console_config = ConsoleConfig {
        server: ConsoleServerConfig {
            // The console does not bind its own listener when embedded (this
            // process owns it); the value is inert, kept only because the type
            // requires it. The public URL's scheme drives the cookie Secure flag.
            bind_address,
            public_base_url: cfg
                .public_base_url
                .clone()
                .unwrap_or_else(|| loopback.clone()),
        },
        switchboard: ConsoleSwitchboardConfig {
            base_url: cfg.api_base_url.clone().unwrap_or(loopback),
        },
    };

    treadmill_console::routes::build_router(ConsoleAppState::new(console_config))
}

pub fn api_router() -> ApiRouter<AppState> {
    ApiRouter::new()
        // OAuth login group (plain routes: browser redirects and the callback are
        // not part of the documented JSON API surface). The {provider} segment
        // selects a configured provider (e.g. `github`).
        //  GET /auth/{provider}/login
        .route("/auth/{provider}/login", get(auth::login))
        //  GET /auth/{provider}/callback
        .route("/auth/{provider}/callback", get(auth::callback))
        //  GET /auth/providers
        .route("/auth/providers", get(auth::providers))
        //  GET /auth/whoami
        .route("/auth/whoami", get(auth::whoami))
        // job management group
        //  POST /jobs -- enqueue a new job
        //  GET  /jobs -- keyset-paginated listing of readable jobs
        .api_route(
            "/jobs",
            post_with(jobs::enqueue, |o| o).get_with(jobs::list, |o| o),
        )
        //  GET /jobs/{id}/events
        .api_route("/jobs/{id}/events", get_with(jobs::list_events, |o| o))
        //  POST /jobs/{id}/log-token -- mint a NATS read token for this job's logs
        .api_route("/jobs/{id}/log-token", post_with(jobs::log_token, |o| o))
        //  GET    /jobs/{id} -- fetch one job's full info
        //  DELETE /jobs/{id} -- request termination of a job
        .api_route(
            "/jobs/{id}",
            get_with(jobs::get_job, |o| o).delete_with(jobs::terminate, |o| o),
        )
        // supervisor management group
        //  GET /supervisors (+ <FILTERS>)
        // .api_route("/supervisors", get_with(supervisors::list, |o| o))
        //  GET /supervisors/{id}/status
        // .api_route(
        //     "/supervisors/{id}/status",
        //     get_with(supervisors::status, |o| o),
        // )
        //  GET /supervisors/{id}/current-job
        //  DELETE /supervisors/{id}/current-job
        //  POST /supervisors/new
        //  DELETE /supervisors/{id}
        //  GET /hosts -- read-only listing of hosts (+ tags, targets, liveness)
        .api_route("/hosts", get_with(hosts::list, |o| o))
        //  GET /hosts/{id}/events
        .api_route("/hosts/{id}/events", get_with(hosts::list_events, |o| o))
        //  GET /hosts/{id}/connect
        // Note that the HTTP verb 'GET' here is not necessarily conformant with REST principles,
        // but is required by RFC6455 §4.1: "The method of the request MUST be GET" (regarding
        // WebSocket HTTP handshakes).
        .api_route("/hosts/{id}/connect", get_with(hosts::connect, |o| o))
        // user management group
        //  GET  /users/me            -- own profile (incl. emails + groups)
        //  PATCH /users/me           -- update display name / username / avatar
        .api_route(
            "/users/me",
            get_with(users::get_me, |o| o).patch_with(users::patch_me, |o| o),
        )
        //  GET /users/me/tokens      -- list own sessions/tokens
        .api_route("/users/me/tokens", get_with(users::list_tokens, |o| o))
        //  DELETE /users/me/tokens/{token_id} -- revoke own token
        .api_route(
            "/users/me/tokens/{token_id}",
            delete_with(users::revoke_token, |o| o),
        )
        //  GET /users/{id}           -- public profile subset
        .api_route("/users/{id}", get_with(users::get_user, |o| o))
        //  GET /users/{id}/events    -- per-user audit feed
        .api_route("/users/{id}/events", get_with(users::list_events, |o| o))
        // image catalog group
        //  POST /images              -- register a concrete image by digest
        //  GET  /images              -- list owned images
        .api_route(
            "/images",
            post_with(images::register_image, |o| o).get_with(images::list_images, |o| o),
        )
        //  GET /images/{digest}      -- inspect one image
        .api_route("/images/{digest}", get_with(images::get_image, |o| o))
        //  POST /image-groups        -- create an empty, named image group
        //  GET  /image-groups        -- list owned image groups
        .api_route(
            "/image-groups",
            post_with(images::create_image_group, |o| o).get_with(images::list_image_groups, |o| o),
        )
        //  GET /image-groups/{id}    -- inspect one image group
        .api_route(
            "/image-groups/{id}",
            get_with(images::get_image_group, |o| o),
        )
        //  POST /image-groups/{id}/generations -- append a full-replacement generation
        .api_route(
            "/image-groups/{id}/generations",
            post_with(images::create_generation, |o| o),
        )
        //  GET /image-groups/{id}/generations/{n} -- inspect one generation
        .api_route(
            "/image-groups/{id}/generations/{n}",
            get_with(images::get_generation, |o| o),
        )
        //  POST /image-groups/{id}/grants -- grant use/manage to a subject
        //  GET  /image-groups/{id}/grants -- list grants
        .api_route(
            "/image-groups/{id}/grants",
            post_with(images::grant_image_group, |o| o)
                .get_with(images::list_image_group_grants, |o| o),
        )
        //  DELETE /image-groups/{id}/grants/{subject_id}/{permission} -- revoke a grant
        .api_route(
            "/image-groups/{id}/grants/{subject_id}/{permission}",
            delete_with(images::revoke_image_group_grant, |o| o),
        )
        //  PUT /image-groups/{id}/public -- toggle the group's implicit `use`
        //  grant to everyone (part of the authorization surface, alongside the
        //  per-subject grants above; not descriptive metadata)
        .api_route(
            "/image-groups/{id}/public",
            put_with(images::set_image_group_public, |o| o),
        )
}

/// Build the OpenAPI document for the client API.
///
/// Shared by the `dump-openapi` binary and the `openapi_spec` drift test so the
/// two entry points cannot diverge. Registers the bearer security scheme that
/// authenticated operations reference via the
/// [`Subject`](crate::auth::Subject) extractor.
pub fn openapi_spec() -> aide::openapi::OpenApi {
    use aide::openapi::{Components, Info, OpenApi, ReferenceOr, SecurityScheme};

    let mut api = OpenApi {
        info: Info {
            title: "Treadmill Switchboard API".to_string(),
            version: "0.1.0".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = api_router().finish_api(&mut api);

    api.components
        .get_or_insert_with(Components::default)
        .security_schemes
        .insert(
            crate::auth::SECURITY_SCHEME.to_string(),
            ReferenceOr::Item(SecurityScheme::Http {
                scheme: "bearer".to_string(),
                bearer_format: None,
                description: Some(
                    "A Treadmill user API token, presented as \
                     `Authorization: Bearer <token>`."
                        .to_string(),
                ),
                extensions: Default::default(),
            }),
        );

    api
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "no such route")
}
