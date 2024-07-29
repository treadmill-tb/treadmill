pub mod jobs;
pub mod supervisors;

use axum::response::{IntoResponse, Response};
use std::fmt::{Debug, Formatter};

pub trait IntoProxiedResponse {
    fn into_proxied_response(self) -> Response;
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
