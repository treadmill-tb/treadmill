//! TODO: CustomLayer that implements tracing_subscriber::Layer for catching specialized event
//!       structs and re-exporting them to database.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceEvent {
    //
}
