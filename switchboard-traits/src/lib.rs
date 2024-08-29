use http::StatusCode;
use serde::{Deserialize, Serialize};

pub trait JsonProxiedStatus: Serialize + for<'de> Deserialize<'de> {
    fn status_code(&self) -> StatusCode;
}
