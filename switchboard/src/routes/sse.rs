//! Server-sent event streams for client resource watches.
//!
//! A watch channel is edge-triggered: each frame is a contentless `change`
//! ping telling the client to re-`GET` the resource (which re-authorizes). The
//! subscription starts woken, so the first ping fires on open and closes the
//! gap between subscribing and the client's first fetch.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::config::ServiceConfig;
use crate::events::{Debounced, WakeSubscription};

/// Build the `text/event-stream` response for a single-resource watch: a
/// leading-edge debounced `change` ping per wake, keepalive comments, and a
/// jittered lifetime cap after which the stream ends and the client reconnects.
pub(crate) fn response(sub: WakeSubscription, config: &ServiceConfig) -> Response {
    let debounced = Debounced::new(vec![sub], config.sse_event_debounce);
    let ttl = jittered(config.sse_channel_ttl);

    let changes = futures_util::stream::unfold(debounced, |mut debounced| async move {
        debounced.wait().await;
        let event = Event::default().event("change").data("{}");
        Some((Ok::<_, Infallible>(event), debounced))
    })
    .take_until(tokio::time::sleep(ttl));

    Sse::new(changes)
        .keep_alive(KeepAlive::new().interval(config.sse_keepalive))
        .into_response()
}

/// `base` scaled by a random factor within ±10%.
fn jittered(base: Duration) -> Duration {
    base.mul_f64(1.0 + (rand::random::<f64>() - 0.5) * 0.2)
}
