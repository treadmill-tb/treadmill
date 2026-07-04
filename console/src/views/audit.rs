//! The reusable audit-feed component.
//!
//! Every page for a resource ends with the head of that resource's audit log,
//! most-recent first. The switchboard already returns rows newest-first
//! (`fetch_events_for_entity` orders by `created_at desc`), so the component
//! renders them in the order received.

use maud::{Markup, html};
use treadmill_rs::api::switchboard::audit::RenderedAuditRow;

use crate::views::timestamp;

/// Render a resource's recent audit events as a titled section. `events` is the
/// head of the reversed log as returned by the switchboard.
pub fn audit_feed(events: &[RenderedAuditRow]) -> Markup {
    html! {
        h2 { "Activity" }
        section.card {
            @if events.is_empty() {
                p.empty { "No recorded activity." }
            } @else {
                table {
                    thead {
                        tr {
                            th { "When" }
                            th { "Event" }
                            th { "Detail" }
                        }
                    }
                    tbody {
                        @for event in events {
                            tr {
                                td { (timestamp(event.created_at)) }
                                td { code { (event.event_type) } }
                                td { (event.message) }
                            }
                        }
                    }
                }
            }
        }
    }
}
