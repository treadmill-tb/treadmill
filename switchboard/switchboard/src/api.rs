pub mod jobs;
pub mod supervisors;

use axum::extract;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};

pub trait IntoProxiedResponse {
    fn into_proxied_response(self) -> Response;
}
pub trait JsonProxiedResponse: Serialize + for<'de> Deserialize<'de> {
    fn status_code(&self) -> StatusCode;
}
impl<T: JsonProxiedResponse> IntoProxiedResponse for T {
    fn into_proxied_response(self) -> Response {
        let status_code = self.status_code();
        (status_code, extract::Json(self)).into_response()
    }
}
#[repr(transparent)]
pub struct ResponseProxy<R: IntoProxiedResponse>(R);
impl<T: IntoProxiedResponse> IntoResponse for ResponseProxy<T> {
    fn into_response(self) -> Response {
        self.0.into_proxied_response()
    }
}
impl<T: IntoProxiedResponse + Debug> Debug for ResponseProxy<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub type Bifurcate<T> = Result<T, T>;
pub type BifurcateProxy<T> = Bifurcate<ResponseProxy<T>>;
macro_rules! bifurcated_ok {
    ($t:expr) => {
        Ok(ResponseProxy($t))
    };
}
use bifurcated_ok;
