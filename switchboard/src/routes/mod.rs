mod auth;
mod hosts;
mod images;
mod jobs;
mod params;
mod users;

use crate::config::ServerConfig;
use crate::serve::AppState;
use aide::axum::ApiRouter;
use aide::axum::routing::{delete_with, get_with, post_with};
use aide::transform::TransformOperation;
use axum::Json;
use axum::Router;
use axum::response::IntoResponse;
use http::{HeaderValue, Method, StatusCode, header};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use treadmill_rs::api::switchboard::images::{ImageInfo, ImageSetGenerationInfo, ImageSetInfo};
use treadmill_rs::api::switchboard::jobs::EnqueueJobResponse;
use treadmill_rs::api::switchboard::{LoginResponse, LoginStagedResponse};

pub fn build_router(state: AppState) -> Router<()> {
    // CORS lets the separately-hosted web console (a browser SPA on another
    // origin) call the API; entries come from `server.cors_allowed_origins`.
    let mut api = api_router().with_state(state.clone());
    if let Some(cors) = cors_layer(&state.config().server) {
        api = api.layer(cors);
    }

    ApiRouter::new()
        // -- INSERT ROUTES HERE --
        .nest_api_service("/api/v1", api)
        // utility
        .fallback(not_found)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .into()
}

/// CORS layer for the API, or `None` (no CORS headers) when no origin is
/// configured. The entries were validated at config load; `"*"` allows any
/// origin. The layer also answers preflight `OPTIONS` requests itself.
fn cors_layer(server: &ServerConfig) -> Option<CorsLayer> {
    let origins = &server.cors_allowed_origins;
    if origins.is_empty() {
        return None;
    }

    let allow_origin = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().map(|o| {
            o.parse::<HeaderValue>()
                .unwrap_or_else(|_| panic!("unvalidated CORS origin: {o:?}"))
        }))
    };

    Some(
        CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PATCH,
                Method::PUT,
                Method::DELETE,
            ])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
            .max_age(Duration::from_secs(3600)),
    )
}

pub fn api_router() -> ApiRouter<AppState> {
    ApiRouter::new()
        // OAuth login group. The {provider} segment selects a configured
        // provider (e.g. `github`).
        //  GET /auth/{provider}/login -- start the flow (redirects to provider)
        .api_route(
            "/auth/{provider}/login",
            get_with(auth::login, |o| {
                doc(o, "startLogin", "Authentication", "Start an OAuth login")
                    .description(auth::AUTH_LOGIN_ENDPOINT_DOC)
                    .response_with::<302, (), _>(|r| {
                        r.description("A redirect to the authentication provider's consent screen.")
                    })
                    .response_with::<400, (), _>(|r| {
                        r.description("The declared return_to is not allowlisted.")
                    })
                    .response_with::<404, (), _>(|r| {
                        r.description("No such provider is configured/enabled.")
                    })
            }),
        )
        //  GET /auth/{provider}/callback -- provider redirect target
        .api_route(
            "/auth/{provider}/callback",
            get_with(auth::callback, |o| {
                doc(
                    o,
                    "finishLogin",
                    "Authentication",
                    "OAuth callback: finish a login",
                )
                .description(auth::AUTH_PROVIDER_CALLBACK_ENDPOINT_DOC)
                .response_with::<200, Json<LoginStagedResponse>, _>(|r| {
                    r.description("The login is staged; complete it at POST /auth/login/complete.")
                })
                .response_with::<302, (), _>(|r| {
                    r.description(
                        "The login is staged; redirect to the original `redirect_to` target.",
                    )
                })
                .response_with::<403, (), _>(|r| {
                    r.description("The identity was denied admission, or the account is locked.")
                })
            }),
        )
        //  GET /auth/providers
        .api_route(
            "/auth/providers",
            get_with(auth::providers, |o| {
                doc(
                    o,
                    "listAuthProviders",
                    "Authentication",
                    "List the enabled login methods",
                )
            }),
        )
        //  GET /auth/whoami
        .api_route(
            "/auth/whoami",
            get_with(auth::whoami, |o| {
                doc(
                    o,
                    "whoAmI",
                    "Authentication",
                    "Get the identity behind the presented token",
                )
            }),
        )
        // Login-completion step:
        //  GET  /auth/tos            -- the current ToS text + version to render
        //  POST /auth/login/complete -- finish a staged login (pending id +
        //                               one-time secret; today: ToS acceptance)
        .api_route(
            "/auth/tos",
            get_with(auth::tos_info, |o| {
                doc(
                    o,
                    "getTermsOfService",
                    "Authentication",
                    "Get the current Terms of Service",
                )
            }),
        )
        .api_route(
            "/auth/login/complete",
            post_with(auth::login_complete, |o| {
                doc(
                    o,
                    "completeLogin",
                    "Authentication",
                    "Complete a staged login",
                )
                .description(auth::AUTH_LOGIN_COMPLETE_ENDPOINT_DOC)
                .response_with::<200, Json<LoginResponse>, _>(|r| {
                    r.description("The login completed; the response carries the session token.")
                })
                .response_with::<403, (), _>(|r| r.description("The account is locked."))
                .response_with::<409, Json<LoginStagedResponse>, _>(|r| {
                    r.description(
                        "A required step is missing, or the echoed tos_version is \
                         not the one in force; the presented pair was consumed and \
                         the marker carries a fresh one to retry with.",
                    )
                })
                .response_with::<410, (), _>(|r| {
                    r.description(
                        "Unknown staged id, wrong secret, or the staged login \
                         expired or was already used.",
                    )
                })
            }),
        )
        // job management group
        //  POST /jobs -- enqueue a new job
        //  GET  /jobs -- keyset-paginated listing of readable jobs
        .api_route(
            "/jobs",
            post_with(jobs::enqueue, |o| {
                doc(o, "enqueueJob", "Jobs", "Enqueue a job")
                    .response_with::<201, Json<EnqueueJobResponse>, _>(|r| {
                        r.description("The job was enqueued; the response carries its id.")
                    })
            })
            .get_with(jobs::list, |o| doc(o, "listJobs", "Jobs", "List jobs")),
        )
        //  GET /jobs/{id}/events
        .api_route(
            "/jobs/{id}/events",
            get_with(jobs::list_events, |o| {
                doc(o, "listJobEvents", "Jobs", "List a job's audit events")
            }),
        )
        //  POST /jobs/{id}/nats-log-token -- mint a NATS read token for this job's logs
        .api_route(
            "/jobs/{id}/nats-log-token",
            post_with(jobs::nats_log_token, |o| {
                doc(
                    o,
                    "createJobNatsLogToken",
                    "Jobs",
                    "Create a NATS log-streaming token for a job",
                )
                .response_with::<503, (), _>(|r| {
                    r.description("Log streaming is not enabled on this deployment.")
                })
            }),
        )
        //  GET    /jobs/{id} -- fetch one job's full info
        //  DELETE /jobs/{id} -- request termination of a job
        .api_route(
            "/jobs/{id}",
            get_with(jobs::get_job, |o| doc(o, "getJob", "Jobs", "Get a job")).delete_with(
                jobs::terminate,
                |o| {
                    doc(o, "terminateJob", "Jobs", "Terminate a job")
                        .response_with::<202, (), _>(|r| {
                            r.description("Termination was initiated.")
                        })
                        .response_with::<204, (), _>(|r| {
                            r.description("The job was already finalized; nothing to do.")
                        })
                },
            ),
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
        .api_route(
            "/hosts",
            get_with(hosts::list, |o| {
                doc(o, "listHosts", "Hosts", "List hosts").description(NOT_PAGINATED)
            }),
        )
        //  GET /hosts/{id}/events
        .api_route(
            "/hosts/{id}/events",
            get_with(hosts::list_events, |o| {
                doc(o, "listHostEvents", "Hosts", "List a host's audit events")
            }),
        )
        //  GET /hosts/{id}/connect
        // Note that the HTTP verb 'GET' here is not necessarily conformant with REST principles,
        // but is required by RFC6455 §4.1: "The method of the request MUST be GET" (regarding
        // WebSocket HTTP handshakes).
        .api_route(
            "/hosts/{id}/connect",
            get_with(hosts::connect, |o| {
                doc(
                    o,
                    "connectHost",
                    "Hosts",
                    "Open a host's supervisor WebSocket",
                )
            }),
        )
        // user management group
        //  GET  /users/me            -- own profile (incl. emails + groups)
        //  PATCH /users/me           -- update display name / username / avatar
        .api_route(
            "/users/me",
            get_with(users::get_me, |o| {
                doc(
                    o,
                    "getCurrentUser",
                    "Users",
                    "Get the current user's profile",
                )
            })
            .patch_with(users::patch_me, |o| {
                doc(
                    o,
                    "updateCurrentUser",
                    "Users",
                    "Update the current user's profile",
                )
                .response_with::<409, (), _>(|r| {
                    r.description("The requested username is already taken.")
                })
            }),
        )
        //  GET /users/me/tokens      -- list own sessions/tokens
        .api_route(
            "/users/me/tokens",
            get_with(users::list_tokens, |o| {
                doc(
                    o,
                    "listCurrentUserTokens",
                    "Users",
                    "List the current user's tokens",
                )
                .description(NOT_PAGINATED)
            }),
        )
        //  DELETE /users/me/tokens/{token_id} -- revoke own token
        .api_route(
            "/users/me/tokens/{token_id}",
            delete_with(users::revoke_token, |o| {
                doc(
                    o,
                    "revokeCurrentUserToken",
                    "Users",
                    "Revoke one of the current user's tokens",
                )
                .response_with::<204, (), _>(|r| r.description("The token was revoked."))
                .response_with::<404, (), _>(|r| {
                    r.description("No such token belonging to the caller.")
                })
            }),
        )
        //  GET /users/{id}           -- public profile subset
        .api_route(
            "/users/{id}",
            get_with(users::get_user, |o| {
                doc(o, "getUser", "Users", "Get a user's public profile")
                    .response_with::<404, (), _>(|r| r.description("No such user."))
            }),
        )
        //  GET /users/{id}/events    -- per-user audit feed
        .api_route(
            "/users/{id}/events",
            get_with(users::list_events, |o| {
                doc(o, "listUserEvents", "Users", "List a user's audit events")
            }),
        )
        // image catalog group
        //  POST /images              -- register a concrete image by digest
        //  GET  /images              -- list owned images
        .api_route(
            "/images",
            post_with(images::register_image, |o| {
                doc(o, "registerImage", "Images", "Register an image")
                    .response_with::<201, Json<ImageInfo>, _>(|r| {
                        r.description("The image was newly registered.")
                    })
                    .response_with::<200, Json<ImageInfo>, _>(|r| {
                        r.description(
                            "The image was already registered; a caller-owned source was added.",
                        )
                    })
                    .response_with::<502, (), _>(|r| {
                        r.description(
                            "The image's registry could not be reached or returned an error.",
                        )
                    })
            })
            .get_with(images::list_images, |o| {
                doc(o, "listImages", "Images", "List images").description(NOT_PAGINATED)
            }),
        )
        //  GET /images/{digest}      -- inspect one image
        .api_route(
            "/images/{digest}",
            get_with(images::get_image, |o| {
                doc(o, "getImage", "Images", "Get an image").response_with::<404, (), _>(|r| {
                    r.description("No such image, or it is not visible to the caller.")
                })
            }),
        )
        //  POST /images/{digest}/sources -- add a registry source (caller-owned)
        .api_route(
            "/images/{digest}/sources",
            post_with(images::add_image_source, |o| {
                doc(o, "addImageSource", "Images", "Add a source to an image")
                    .response_with::<201, Json<ImageInfo>, _>(|r| {
                        r.description("The source was added; the caller owns it.")
                    })
                    .response_with::<404, (), _>(|r| r.description("No such registered image."))
                    .response_with::<502, (), _>(|r| {
                        r.description(
                            "The source could not be reached or does not serve the image.",
                        )
                    })
            }),
        )
        //  DELETE /images/{digest}/sources/{source_id} -- delete a source
        .api_route(
            "/images/{digest}/sources/{source_id}",
            delete_with(images::delete_image_source, |o| {
                doc(o, "deleteImageSource", "Images", "Delete an image source")
                    .response_with::<204, (), _>(|r| r.description("The source was deleted."))
                    .response_with::<404, (), _>(|r| r.description("No such image or source."))
            }),
        )
        //  POST /images/{digest}/sources/{source_id}/grants -- grant use/manage
        //  GET  /images/{digest}/sources/{source_id}/grants -- list grants
        .api_route(
            "/images/{digest}/sources/{source_id}/grants",
            post_with(images::grant_image_source, |o| {
                doc(
                    o,
                    "createImageSourceGrant",
                    "Images",
                    "Grant a permission on an image source",
                )
                .response_with::<204, (), _>(|r| r.description("The grant was recorded."))
                .response_with::<404, (), _>(|r| r.description("No such image or source."))
            })
            .get_with(images::list_image_source_grants, |o| {
                doc(
                    o,
                    "listImageSourceGrants",
                    "Images",
                    "List an image source's grants",
                )
                .description(NOT_PAGINATED)
            }),
        )
        //  DELETE /images/{digest}/sources/{source_id}/grants/{subject_id}/{permission}
        .api_route(
            "/images/{digest}/sources/{source_id}/grants/{subject_id}/{permission}",
            delete_with(images::revoke_image_source_grant, |o| {
                doc(
                    o,
                    "revokeImageSourceGrant",
                    "Images",
                    "Revoke a grant on an image source",
                )
                .response_with::<204, (), _>(|r| r.description("The grant was revoked."))
                .response_with::<404, (), _>(|r| r.description("No matching grant to revoke."))
            }),
        )
        //  POST /image-sets        -- create an empty, named image set
        //  GET  /image-sets        -- list owned image sets
        .api_route(
            "/image-sets",
            post_with(images::create_image_set, |o| {
                doc(o, "createImageSet", "Image sets", "Create an image set")
                    .response_with::<201, Json<ImageSetInfo>, _>(|r| {
                        r.description("The image set was created.")
                    })
                    .response_with::<409, (), _>(|r| {
                        r.description("An image set with that name already exists.")
                    })
            })
            .get_with(images::list_image_sets, |o| {
                doc(o, "listImageSets", "Image sets", "List image sets").description(NOT_PAGINATED)
            }),
        )
        //  GET /image-sets/{id}    -- inspect one image set
        .api_route(
            "/image-sets/{id}",
            get_with(images::get_image_set, |o| {
                doc(o, "getImageSet", "Image sets", "Get an image set").response_with::<404, (), _>(
                    |r| r.description("No such image set, or it is not visible to the caller."),
                )
            }),
        )
        //  GET /image-sets/{id}/events -- the set's audit feed (manage-gated)
        .api_route(
            "/image-sets/{id}/events",
            get_with(images::list_events, |o| {
                doc(
                    o,
                    "listImageSetEvents",
                    "Image sets",
                    "List an image set's audit events",
                )
                .response_with::<404, (), _>(|r| {
                    r.description("No such image set, or it is not visible to the caller.")
                })
            }),
        )
        //  POST /image-sets/{id}/generations -- append a full-replacement generation
        .api_route(
            "/image-sets/{id}/generations",
            post_with(images::create_generation, |o| {
                doc(
                    o,
                    "createImageSetGeneration",
                    "Image sets",
                    "Append a generation to an image set",
                )
                .response_with::<201, Json<ImageSetGenerationInfo>, _>(|r| {
                    r.description("The generation was appended.")
                })
                .response_with::<404, (), _>(|r| {
                    r.description("No such image set, or it is not visible to the caller.")
                })
            }),
        )
        //  GET /image-sets/{id}/generations/{n} -- inspect one generation
        .api_route(
            "/image-sets/{id}/generations/{n}",
            get_with(images::get_generation, |o| {
                doc(
                    o,
                    "getImageSetGeneration",
                    "Image sets",
                    "Get an image-set generation",
                )
                .response_with::<404, (), _>(|r| r.description("No such image set or generation."))
            }),
        )
        //  POST /image-sets/{id}/grants -- grant use/manage to a subject
        //  GET  /image-sets/{id}/grants -- list grants
        .api_route(
            "/image-sets/{id}/grants",
            post_with(images::grant_image_set, |o| {
                doc(
                    o,
                    "createImageSetGrant",
                    "Image sets",
                    "Grant a permission on an image set",
                )
                .response_with::<204, (), _>(|r| r.description("The grant was recorded."))
                .response_with::<404, (), _>(|r| {
                    r.description("No such image set, or it is not visible to the caller.")
                })
            })
            .get_with(images::list_image_set_grants, |o| {
                doc(
                    o,
                    "listImageSetGrants",
                    "Image sets",
                    "List an image set's grants",
                )
                .description(NOT_PAGINATED)
            }),
        )
        //  DELETE /image-sets/{id}/grants/{subject_id}/{permission} -- revoke a grant
        .api_route(
            "/image-sets/{id}/grants/{subject_id}/{permission}",
            delete_with(images::revoke_image_set_grant, |o| {
                doc(
                    o,
                    "revokeImageSetGrant",
                    "Image sets",
                    "Revoke a grant on an image set",
                )
                .response_with::<204, (), _>(|r| r.description("The grant was revoked."))
                .response_with::<404, (), _>(|r| r.description("No matching grant to revoke."))
            }),
        )
    // "Public" is not a dedicated route: a set is made public by granting the
    // well-known `everyone` subject `use` via the grant routes above.
}

/// Operation description for the list routes that return their whole result
/// set in one response (unlike `GET /jobs` and the audit feeds, which page).
const NOT_PAGINATED: &str =
    "Returns the complete set in a stable order; this route is not paginated.";

/// Set the operationId, tag, and summary shared by a documented operation.
/// `operation_id` is camelCase; generators re-case it to the target language.
fn doc<'a>(
    op: TransformOperation<'a>,
    operation_id: &str,
    tag: &str,
    summary: &str,
) -> TransformOperation<'a> {
    op.id(operation_id).tag(tag).summary(summary)
}

/// Build the OpenAPI document for the client API.
///
/// Shared by the `dump-openapi` binary and the `openapi_spec` drift test so the
/// two entry points cannot diverge. Registers the bearer security scheme that
/// authenticated operations reference via the
/// [`Subject`](crate::auth::Subject) extractor.
pub fn openapi_spec() -> aide::openapi::OpenApi {
    use aide::openapi::{Components, Info, OpenApi, ReferenceOr, SecurityScheme, Tag};

    let tag = |name: &str, description: &str| Tag {
        name: name.to_string(),
        description: Some(description.to_string()),
        ..Default::default()
    };

    let mut api = OpenApi {
        info: Info {
            title: "Treadmill Switchboard API".to_string(),
            version: "0.1.0".to_string(),
            ..Default::default()
        },
        tags: vec![
            tag(
                "Authentication",
                "Interactive OAuth login, the login-completion step, and token introspection.",
            ),
            tag("Jobs", "Enqueue, inspect, and terminate jobs."),
            tag("Hosts", "Inspect hosts and their attached targets."),
            tag("Images", "Register and inspect catalog images."),
            tag(
                "Image sets",
                "Manage image sets, their generations, and grants.",
            ),
            tag("Users", "Profiles, sessions, and audit feeds."),
        ],
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
