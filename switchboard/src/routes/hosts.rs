use crate::audit::feed::{AuditFeedQuery, AuditFeedResponse, fetch_events_for_entity};
use axum::Json;
use axum::extract::Query;
use std::collections::HashMap;
use std::net::SocketAddr;

use treadmill_rs::api::switchboard::JobInitSpec;
use treadmill_rs::api::switchboard::hosts::{
    HostCreateRequest, HostCreateResponse, HostInfo, HostRequirementsReport,
    HostRequirementsRequest, HostSpecRejection, HostSpecUpdateRequest, HostSpecUpdateResponse,
    HostUpdateRequest,
};
use treadmill_rs::host_spec::{HostSpec, HostSpecV1};

/// Axum handler for the `/hosts/{id}/events` path.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: host_id }): Path<IdPath>,
    Query(query): Query<AuditFeedQuery>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "host", host_id, &query)
        .await
        .map(Json)
}

/// Axum handler for `GET /hosts/{id}/watch`.
///
/// Opens a Server-Sent Events stream that pings whenever the host's row
/// changes, gated once at open on the caller's `read` permission (403
/// otherwise, including for a nonexistent host). Each ping is contentless; the
/// client re-`GET`s the host in response.
pub async fn watch(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: host_id }): Path<IdPath>,
) -> Result<Response, StatusCode> {
    use crate::auth::engine::{self, HostPermission};

    let authorized = engine::can_access_host(
        state.pool(),
        subject.user_id(),
        host_id,
        HostPermission::Read,
    )
    .await
    .or_internal("checking host read access for a watch stream")?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let sub = state.event_bus().subscribe(&[EventFilter {
        table: "hosts",
        key: Some(("host_id", host_id)),
    }]);
    Ok(crate::routes::sse::response(sub, &state.config().service))
}

/// Axum handler for `PATCH /hosts/{id}` — change a host's operational state.
///
/// Requires `manage` on the host, the meta-permission its owner holds
/// implicitly. A request that changes nothing (absent field, or the value
/// already in force) is a no-op: no write, no audit event, and no change
/// notification for watchers.
pub async fn update(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: host_id }): Path<IdPath>,
    Json(req): Json<HostUpdateRequest>,
) -> Result<Response, StatusCode> {
    use crate::audit::model::{Host as AuditHost, Subject as AuditSubject};
    use crate::audit::{self, events};
    use crate::auth::engine::{self, HostPermission};

    let authorized = engine::can_access_host(
        state.pool(),
        subject.user_id(),
        host_id,
        HostPermission::Manage,
    )
    .await
    .or_internal("checking host manage access for update")?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let Some(maintenance) = req.maintenance else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };

    let mut txn = state
        .pool()
        .begin()
        .await
        .or_internal(&format!("opening a transaction to update host {host_id}"))?;

    let previous = sql::host::lock_maintenance(host_id, &mut txn)
        .await
        .or_internal(&format!("reading maintenance of host {host_id}"))?;
    // `can_access_host` already refused an unreadable host, so a missing row
    // here means it was deleted in between.
    let Some(previous) = previous else {
        return Err(StatusCode::NOT_FOUND);
    };
    if previous == maintenance {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    sql::host::set_maintenance(host_id, maintenance, &mut txn)
        .await
        .or_internal(&format!("setting maintenance on host {host_id}"))?;
    audit::emit(
        &mut txn,
        &events::HostMaintenanceChanged {
            actor: AuditSubject(subject.user_id()),
            host: AuditHost(host_id),
            maintenance,
        },
    )
    .await
    .or_internal("recording a host maintenance change")?;
    txn.commit()
        .await
        .or_internal(&format!("committing the update of host {host_id}"))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Axum handler for `GET /host-spec/schema` — the JSON Schema of a host spec.
///
/// The same artifact as the committed `host_spec.schema.json` snapshot, served
/// so the console can render an editor and a field reference from it instead
/// of vendoring a copy that would drift. schemars lifts the Rust type's
/// rustdoc into `description`, which makes the type the single source for the
/// validator, the CEL environment and the UI copy alike.
pub async fn spec_schema() -> Json<serde_json::Value> {
    static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
        serde_json::to_value(schemars::schema_for!(HostSpec)).expect("the spec schema serializes")
    });
    Json(SCHEMA.clone())
}

/// Axum handler for `POST /host-requirements/validate` — a dry run of a job's
/// host requirements.
///
/// Reports over the hosts the *caller* may start on. An owner cannot be named
/// the way enqueue allows: the caller's own authorization is what bounds the
/// counts, and letting it be widened is exactly how this would become a probe
/// for hosts the caller cannot see.
pub async fn validate_requirements(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<HostRequirementsRequest>,
) -> Result<Json<HostRequirementsReport>, StatusCode> {
    use crate::auth::engine::{self, ImageSetPermission};

    // Resolve the image set exactly as enqueue does, so the report describes
    // the membership an actual submission would freeze.
    let image_set = match req.init_spec {
        Some(JobInitSpec::ImageSet { set_id, generation }) => {
            let may_use = engine::can_access_image_set(
                state.pool(),
                subject.user_id(),
                set_id,
                ImageSetPermission::Use,
            )
            .await
            .or_internal(&format!("checking `use` on image set {set_id}"))?;
            if !may_use {
                return Err(StatusCode::FORBIDDEN);
            }
            let resolved = match generation {
                Some(g) => Some(g),
                None => crate::sql::image::latest_generation(state.pool(), set_id)
                    .await
                    .or_internal(&format!("resolving latest generation of set {set_id}"))?,
            };
            // A set with no generation has nothing to select from, which is a
            // property of the request rather than a server fault.
            Some((set_id, resolved.ok_or(StatusCode::BAD_REQUEST)?))
        }
        // A concrete image constrains no host, and resume/restart inherit the
        // predecessor's image; in both cases only the predicate is evaluated.
        _ => None,
    };

    let report = crate::host_requirements::evaluate(
        state.pool(),
        subject.user_id(),
        &req.host_cel_predicate,
        image_set,
    )
    .await
    .or_internal("evaluating host requirements")?;

    Ok(Json(report))
}

/// Validate a submitted spec document.
///
/// The version is probed first and each version deserialized as its own type:
/// going through the untagged [`HostSpec`] would report every failure at the
/// document root. `serde_path_to_error` then names the offending field rather
/// than a byte offset, which for a hand-edited document is the difference
/// between a usable error and a puzzle.
fn validate_spec(document: serde_json::Value) -> Result<HostSpecV1, HostSpecRejection> {
    let rejection = |path: &str, message: String| HostSpecRejection {
        path: path.to_string(),
        message,
    };
    match document.get("spec_version").and_then(|v| v.as_str()) {
        Some("v1") => serde_path_to_error::deserialize::<_, HostSpecV1>(document).map_err(|e| {
            HostSpecRejection {
                path: e.path().to_string(),
                message: e.into_inner().to_string(),
            }
        }),
        Some(other) => Err(rejection(
            "spec_version",
            format!("unknown spec version `{other}`"),
        )),
        None => Err(rejection("spec_version", "missing".to_string())),
    }
}

fn refuse(rejection: HostSpecRejection) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(rejection)).into_response()
}

/// Axum handler for `POST /hosts` — admit a host to the fleet.
///
/// Global-admin only: this mints a supervisor credential and puts a machine
/// into scheduling. The `hosts` row and revision 1 of its spec are written in
/// one transaction, which is what makes "every host has a spec" hold — SQL
/// cannot require a child row.
///
/// The client supplies the host's UUID as the spec's `id`, so a spec is a
/// self-contained document; the switchboard enforces uniqueness.
pub async fn create(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(req): Json<HostCreateRequest>,
) -> Result<Response, StatusCode> {
    use crate::audit::model::{Host as AuditHost, Subject as AuditSubject};
    use crate::audit::{self, events};
    use crate::auth::engine;

    let admin = engine::is_admin(state.pool(), subject.user_id())
        .await
        .or_internal("checking admin for host creation")?;
    if !admin {
        return Err(StatusCode::FORBIDDEN);
    }

    let spec = match validate_spec(req.spec) {
        Ok(spec) => spec,
        Err(rejection) => return Ok(refuse(rejection)),
    };
    let host_id = spec.id;
    let name = spec.name.clone();

    let auth_token = SecurityToken::generate();
    let presented = auth_token.to_string();

    let mut txn = state
        .pool()
        .begin()
        .await
        .or_internal("opening a transaction to create a host")?;

    match sql::host::insert(host_id, name.clone(), auth_token, &mut *txn).await {
        Ok(()) => {}
        // The id or the generated token collided; only the former is plausible.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            tracing::debug!("refusing to create host {host_id}: already exists");
            return Ok(StatusCode::CONFLICT.into_response());
        }
        Err(e) => return Err(crate::http_error::internal(e)),
    }
    sql::host_spec::insert_revision(
        host_id,
        FIRST_SPEC_REVISION,
        &HostSpec::V1(spec),
        Some(subject.user_id()),
        &mut *txn,
    )
    .await
    .or_internal(&format!("writing the first spec of host {host_id}"))?;
    audit::emit(
        &mut txn,
        &events::HostCreated {
            actor: AuditSubject(subject.user_id()),
            host: AuditHost(host_id),
            name,
        },
    )
    .await
    .or_internal("recording a host creation")?;
    txn.commit()
        .await
        .or_internal(&format!("committing the creation of host {host_id}"))?;

    Ok((
        StatusCode::CREATED,
        Json(HostCreateResponse {
            host_id,
            auth_token: presented,
            spec_revision: FIRST_SPEC_REVISION,
        }),
    )
        .into_response())
}

/// The revision a host's first spec is written at.
const FIRST_SPEC_REVISION: i32 = 1;

/// Axum handler for `PUT /hosts/{id}/spec` — store a new revision of a host's
/// spec.
///
/// Requires `manage`, the same meta-permission that governs the host's
/// operational state. Conditional on `If-Match` carrying the revision the
/// caller last read: two admins editing one host must not silently clobber
/// each other. The primary key is the compare-and-swap — the write inserts at
/// `expected + 1`, so a racing writer that computed the same number takes a
/// unique violation rather than overwriting.
pub async fn put_spec(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: host_id }): Path<IdPath>,
    headers: HeaderMap,
    Json(req): Json<HostSpecUpdateRequest>,
) -> Result<Response, StatusCode> {
    use crate::audit::model::{Host as AuditHost, Subject as AuditSubject};
    use crate::audit::{self, events};
    use crate::auth::engine::{self, HostPermission};

    let authorized = engine::can_access_host(
        state.pool(),
        subject.user_id(),
        host_id,
        HostPermission::Manage,
    )
    .await
    .or_internal("checking host manage access for a spec write")?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let Some(expected) = if_match_revision(&headers) else {
        tracing::debug!("refusing an unconditional spec write for host {host_id}");
        return Ok(StatusCode::PRECONDITION_REQUIRED.into_response());
    };

    let spec = match validate_spec(req.spec) {
        Ok(spec) => spec,
        Err(rejection) => return Ok(refuse(rejection)),
    };
    // Also a table constraint; checked here so the caller gets a field path
    // instead of a 500 from a violated CHECK.
    if spec.id != host_id {
        return Ok(refuse(HostSpecRejection {
            path: "id".to_string(),
            message: format!("must be the host being written, {host_id}"),
        }));
    }

    let mut txn = state
        .pool()
        .begin()
        .await
        .or_internal("opening a transaction to write a host spec")?;

    let current = sql::host_spec::current_revision(host_id, &mut *txn)
        .await
        .or_internal(&format!("reading the current spec revision of {host_id}"))?;
    // Catches a stale reader and a caller inventing a future revision alike;
    // the unique violation below catches the race this check cannot see.
    if current != Some(expected) {
        tracing::debug!(
            "refusing a spec write for host {host_id}: If-Match {expected}, current {current:?}"
        );
        return Ok(StatusCode::PRECONDITION_FAILED.into_response());
    }

    let revision = expected + 1;
    match sql::host_spec::insert_revision(
        host_id,
        revision,
        &HostSpec::V1(spec),
        Some(subject.user_id()),
        &mut *txn,
    )
    .await
    {
        Ok(()) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            tracing::debug!("lost a spec write race for host {host_id} at revision {revision}");
            return Ok(StatusCode::PRECONDITION_FAILED.into_response());
        }
        Err(e) => return Err(crate::http_error::internal(e)),
    }
    audit::emit(
        &mut txn,
        &events::HostSpecUpdated {
            actor: AuditSubject(subject.user_id()),
            host: AuditHost(host_id),
            revision,
        },
    )
    .await
    .or_internal("recording a host spec write")?;
    txn.commit()
        .await
        .or_internal(&format!("committing spec revision {revision} of {host_id}"))?;

    Ok(Json(HostSpecUpdateResponse {
        spec_revision: revision,
    })
    .into_response())
}

/// The revision an `If-Match` header names, or `None` if it is absent or not a
/// revision.
///
/// Revisions are integers, so the entity tag is the number, optionally quoted
/// as `ETag` syntax proper. A weak validator (`W/"3"`) is not accepted: this is
/// a compare-and-swap, and weak comparison is explicitly not that.
fn if_match_revision(headers: &HeaderMap) -> Option<i32> {
    headers
        .get(http::header::IF_MATCH)?
        .to_str()
        .ok()?
        .trim()
        .trim_matches('"')
        .parse()
        .ok()
}

/// Axum handler for `GET /hosts/{id}` — one host with its spec.
///
/// Scoped to `read`, like the listing. A host the caller cannot read is a 403
/// rather than a 404, so the route does not leak which ids exist.
pub async fn get(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: host_id }): Path<IdPath>,
) -> Result<Json<HostInfo>, StatusCode> {
    use crate::auth::engine::{self, HostPermission};

    let authorized = engine::can_access_host(
        state.pool(),
        subject.user_id(),
        host_id,
        HostPermission::Read,
    )
    .await
    .or_internal("checking host read access")?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let host = sql::host::fetch_listing(host_id, state.pool())
        .await
        .or_internal(&format!("reading host {host_id}"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    let spec = match sql::host_spec::current_for_host(host_id, state.pool())
        .await
        .or_internal(&format!("reading the spec of host {host_id}"))?
    {
        Some(Ok(stored)) => Some((stored.revision, stored.spec)),
        // A document this build cannot read is reported as no spec rather than
        // failing the whole request; the listing does the same.
        Some(Err(e)) => {
            tracing::error!("omitting host spec: {e}");
            None
        }
        None => None,
    };

    Ok(Json(host_info(host, spec, &state)))
}

/// Assemble the client view of a host from its row and current spec.
fn host_info(
    host: sql::host::SqlHostListing,
    spec: Option<(i32, HostSpec)>,
    state: &AppState,
) -> HostInfo {
    let cutoff = chrono::Utc::now() - state.config().service.host_liveness_timeout;
    let (spec_revision, spec) = match spec {
        // Normalize on read: nothing downstream sees an old version.
        Some((revision, spec)) => (Some(revision), Some(HostSpec::V1(spec.into_latest()))),
        None => (None, None),
    };
    HostInfo {
        live: host.last_seen_at.is_some_and(|t| t > cutoff),
        host_id: host.host_id,
        name: host.name,
        maintenance: host.maintenance,
        last_seen_at: host.last_seen_at,
        spec,
        spec_revision,
    }
}

/// Axum handler for `GET /hosts` — the hosts the caller may read, each with
/// its spec.
///
/// Scoped to `read`: a spec is visible to anyone who can read its host, so the
/// listing cannot show every host the way the tag view did. Liveness is
/// computed against the same heartbeat window the scheduler uses
/// (`host_liveness_timeout`).
pub async fn list(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
) -> Result<Json<Vec<HostInfo>>, StatusCode> {
    let hosts = crate::sql::host::list_readable(subject.user_id(), state.pool())
        .await
        .or_internal("listing hosts")?;

    // One query for every current spec, rather than one per host.
    let mut specs: HashMap<Uuid, (i32, HostSpec)> =
        crate::sql::host_spec::current_for_all_hosts(state.pool())
            .await
            .or_internal("listing host specs")?
            .into_iter()
            .filter_map(|row| match row {
                Ok(stored) => Some((stored.host_id, (stored.revision, stored.spec))),
                Err(e) => {
                    tracing::error!("omitting host spec from listing: {e}");
                    None
                }
            })
            .collect();

    let out = hosts
        .into_iter()
        .map(|h| {
            let spec = specs.remove(&h.host_id);
            host_info(h, spec, &state)
        })
        .collect();

    Ok(Json(out))
}

use axum::extract::Path;
use axum::extract::State;
use axum::extract::{ConnectInfo, WebSocketUpgrade, ws};
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;

use headers::Authorization;
use headers::authorization::Bearer;

use http::{HeaderMap, HeaderValue, StatusCode};

use tracing::instrument;

use treadmill_rs::api::switchboard_supervisor::websocket::{
    TREADMILL_PROTOCOL_MINOR_HEADER, TREADMILL_WEBSOCKET_CONFIG, TREADMILL_WEBSOCKET_PROTOCOL,
};
use treadmill_rs::api::switchboard_supervisor::{ProtocolVersion, ServerHello};

use uuid::Uuid;

use crate::auth::token::SecurityToken;
use crate::events::EventFilter;
use crate::http_error::OrInternal;
use crate::routes::params::IdPath;
use crate::serve::AppState;
use crate::sql;
use crate::supervisor_ws_worker::{SupervisorWSWorker, SupervisorWSWorkerConfig};

// -- connect

/// Axum handler for the `/hosts/{id}/connect` path.
///
/// Responds with an `Upgrade: websocket` and launches the supervisor worker as
/// a `tokio` task.
// `skip_all`: the remaining arguments are the bearer token and the raw request
// headers, which must not become span fields.
#[instrument(skip_all, fields(host_id = %host_id))]
pub async fn connect(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Path(IdPath { id: host_id }): Path<IdPath>,
    headers: HeaderMap,
) -> Response {
    let auth_token = match SecurityToken::try_from(bearer) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("Failed to extract bearer token: {e}");
            return StatusCode::FORBIDDEN.into_response();
        }
    };

    let auth_result = sql::host::try_authenticate_for_host(host_id, auth_token, state.pool()).await;
    match auth_result {
        Ok(true) => (), // Success!
        Ok(false) => {
            tracing::warn!("invalid token presented for host ({host_id})");
            return StatusCode::FORBIDDEN.into_response();
        }
        Err(e) => {
            tracing::error!("failed to authenticate supervisor for host ({host_id}): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    /// Check that the WebSocket subprotocol is correctly specified as `treadmill`.
    fn check_protocol_header(protocol: Option<&HeaderValue>, host_id: Uuid) -> bool {
        if let Some(protocol) = protocol {
            if protocol != HeaderValue::from_static(TREADMILL_WEBSOCKET_PROTOCOL) {
                tracing::warn!(
                    "Websocket connection for host ({host_id}) specifies \
		     `Sec-Websocket-Protocol: {protocol:?}`, which is not \
		     recognized. Closing."
                );
                false
            } else {
                true
            }
        } else {
            tracing::warn!(
                "Websocket connection for host ({host_id}) does not specify \
		 Sec-Websocket-Protocol, closing."
            );
            false
        }
    }

    // Minor-version negotiation: the supervisor advertises its protocol minor
    // in a request header (absent ⇒ treat as 0, i.e. an older peer). The
    // effective minor for this connection is the lower of the two; neither side
    // may emit a feature/variant introduced above it.
    let supervisor_minor: u16 = headers
        .get(TREADMILL_PROTOCOL_MINOR_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `min` is a no-op while PROTOCOL_MINOR is 0, but it is the correct
    // negotiation once minors diverge; keep it rather than hard-code 0.
    #[allow(clippy::unnecessary_min_or_max)]
    let effective_minor = supervisor_minor.min(ProtocolVersion::CURRENT.minor);
    tracing::info!(
        supervisor_minor,
        server_minor = ProtocolVersion::CURRENT.minor,
        effective_minor,
        "Negotiated protocol minor with supervisor for host ({host_id})."
    );

    let worker_pool = state.pool().clone();

    let ws_worker_config = SupervisorWSWorkerConfig {
        supervisor_ping_interval: state.config().service.supervisor_ping_interval,
        supervisor_pong_dead: state.config().service.supervisor_pong_dead,
        supervisor_reconcile_interval: state.config().service.supervisor_reconcile_interval,
        supervisor_event_debounce: state.config().service.supervisor_event_debounce,
    };

    // Shared with the worker so it can mint per-job write tokens and provision
    // streams at dispatch; `None` when log streaming is disabled.
    let log_streaming = state.log_streaming().cloned();
    // Likewise for the gateway material the worker hands a job at dispatch;
    // `None` when the deployment runs without a gateway.
    let job_gateway = state.job_gateway().cloned();
    let event_bus = state.event_bus().clone();

    let mut response =
        ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL])
            .on_upgrade(move |mut web_socket| async move {
                tokio::spawn(async move {
                    // Resolve the subprotocol check into an owned `bool` before
                    // any `.await`: `protocol()` borrows `web_socket`, and that
                    // borrow must not straddle the send below — a live
                    // `&WebSocket` across an await would force the spawned future
                    // to require `WebSocket: Sync`, which it isn't.
                    let wrong_protocol = !check_protocol_header(web_socket.protocol(), host_id);
                    if wrong_protocol {
                        if let Err(e) = web_socket.send(ws::Message::Close(None)).await {
                            tracing::warn!(
                                "Failed to send close frame (wrong subprotocol) \
			     for host ({host_id}): {e}."
                            );
                        }
                        return;
                    }

                    tracing::info!("Starting SupervisorWSWorker for host ({host_id}).");
                    tracing::debug!("Host ({host_id}) is connecting from {socket_addr}.");

                    SupervisorWSWorker::run(
                        worker_pool,
                        host_id,
                        web_socket,
                        ws_worker_config,
                        log_streaming,
                        job_gateway,
                        event_bus,
                    )
                    .await
                });
            });

    // The host's own description, handed back as the admin wrote it. Read
    // outside the upgrade closure because the hello travels in a response
    // header, which is fixed before the socket exists.
    let host_spec = match sql::host_spec::current_for_host(host_id, state.pool()).await {
        Ok(Some(Ok(stored))) => Some(HostSpec::V1(stored.normalize())),
        Ok(None) => None,
        Ok(Some(Err(e))) => {
            tracing::error!("host {host_id} connecting with an unreadable spec: {e}");
            None
        }
        Err(e) => {
            tracing::error!("failed to read the spec of connecting host {host_id}: {e}");
            None
        }
    };

    let hello = ServerHello {
        protocol: ProtocolVersion::CURRENT,
        features: Default::default(),
        host_spec,
    };
    let (header, spec_dropped) = socket_config_header(hello);
    if spec_dropped {
        tracing::error!(
            "host {host_id}: its spec does not fit the socket-config header budget; \
             connecting without it"
        );
    }
    match header {
        Some(value) => {
            response
                .headers_mut()
                .insert(TREADMILL_WEBSOCKET_CONFIG, value);
        }
        // Unreachable: a spec-less hello is a short ASCII document.
        None => tracing::error!("host {host_id}: could not encode a socket config header"),
    }

    response
}

/// Budget for the serialized [`ServerHello`], which travels in a response
/// header.
///
/// `HeaderValue` itself imposes no length limit, so nothing below this line
/// would reject an oversized document — but hyper caps a whole header block at
/// 16 KiB by default, and intermediaries cap lower. Exceeding that makes the
/// handshake response unreadable, and a supervisor that cannot read it cannot
/// connect at all. A spec is admin-authored and unbounded (a host may list
/// arbitrarily many DUTs), so the ceiling has to be enforced here.
const MAX_SOCKET_CONFIG_BYTES: usize = 8 * 1024;

/// Encode a hello for the socket-config header, dropping the spec if the full
/// document exceeds [`MAX_SOCKET_CONFIG_BYTES`].
///
/// Returns whether the spec had to be dropped. Connecting without a
/// description is how the supervisor behaved before the field existed, and is
/// a far better outcome than refusing the connection.
fn socket_config_header(hello: ServerHello) -> (Option<HeaderValue>, bool) {
    fn encode(hello: &ServerHello) -> Option<HeaderValue> {
        let json = serde_json::to_string(hello).ok()?;
        if json.len() > MAX_SOCKET_CONFIG_BYTES {
            return None;
        }
        json.parse().ok()
    }

    if hello.host_spec.is_some()
        && let Some(value) = encode(&hello)
    {
        return (Some(value), false);
    }
    let dropped = hello.host_spec.is_some();
    (
        encode(&ServerHello {
            host_spec: None,
            ..hello
        }),
        dropped,
    )
}

#[cfg(test)]
mod tests {
    use treadmill_rs::host_spec::{Dut, HostSpecV1, Platform, Resources, SpecVersionV1};

    use super::*;

    fn hello(duts: usize) -> ServerHello {
        ServerHello {
            protocol: ProtocolVersion::CURRENT,
            features: Default::default(),
            host_spec: Some(HostSpec::V1(HostSpecV1 {
                spec_version: SpecVersionV1::V1,
                id: Uuid::nil(),
                name: "cam-rpi4-01".into(),
                description: None,
                site: "cambridge".into(),
                location: None,
                platform: Platform::Physical {
                    arch: "aarch64".into(),
                    profiles: vec!["rpi4-uboot-sd".into()],
                    vendor: "Raspberry Pi Ltd".into(),
                    model: "Raspberry Pi 4 Model B".into(),
                },
                resources: Resources {
                    cpu_cores: 4,
                    memory_mb: 8192,
                    storage_gb: 64,
                },
                labels: Default::default(),
                duts: (0..duts)
                    .map(|i| Dut {
                        name: Some(format!("nRF52840-DK #{i}")),
                        serial: Some(format!("10501234{i:02}")),
                        vendor: "Nordic Semiconductor".into(),
                        board: "nrf52840dk".into(),
                        arch: vec!["cortex-m4".into()],
                        connectivity: vec!["ble".into(), "usb".into()],
                        debug: None,
                        console: None,
                        labels: Default::default(),
                    })
                    .collect(),
            })),
        }
    }

    fn decode(value: &HeaderValue) -> ServerHello {
        serde_json::from_str(value.to_str().expect("the header is text"))
            .expect("the header is a hello")
    }

    #[test]
    fn a_realistic_spec_travels_in_the_header() {
        let (header, dropped) = socket_config_header(hello(4));
        assert!(!dropped);
        let decoded = decode(&header.expect("encodes"));
        let HostSpec::V1(spec) = decoded.host_spec.expect("the spec is carried");
        assert_eq!(spec.site, "cambridge");
        assert_eq!(spec.duts.len(), 4);
    }

    /// The handshake must survive a spec too large for a header: a supervisor
    /// that cannot read the response cannot connect at all.
    #[test]
    fn an_oversized_spec_is_dropped_rather_than_breaking_the_handshake() {
        let (header, dropped) = socket_config_header(hello(500));
        assert!(dropped);
        let header = header.expect("a spec-less hello still encodes");
        assert!(header.len() <= MAX_SOCKET_CONFIG_BYTES);
        let decoded = decode(&header);
        assert!(decoded.host_spec.is_none());
        // The rest of the handshake is intact, so negotiation still works.
        assert_eq!(decoded.protocol, ProtocolVersion::CURRENT);
    }

    /// An undescribed host is not an error path; it just carries no spec.
    #[test]
    fn a_host_without_a_spec_reports_nothing_dropped() {
        let (header, dropped) = socket_config_header(ServerHello {
            host_spec: None,
            ..hello(0)
        });
        assert!(!dropped);
        assert!(decode(&header.expect("encodes")).host_spec.is_none());
    }
}
