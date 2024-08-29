use crate::auth::AuthorizationError;
use axum::extract;
use axum::response::{IntoResponse, Response};
use std::fmt::{Debug, Formatter};
pub use tml_switchboard_traits::JsonProxiedStatus;

pub trait IntoProxiedResponse {
    fn into_proxied_response(self) -> Response;
}
impl<T: JsonProxiedStatus> IntoProxiedResponse for T {
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

pub fn proxy_err<E, T: IntoProxiedResponse + From<E>>(err: E) -> ResponseProxy<T> {
    ResponseProxy(T::from(err))
}

pub type Proxied<T> = Result<ResponseProxy<T>, ResponseProxy<T>>;
pub fn proxy<T: IntoProxiedResponse>(val: Result<T, T>) -> Proxied<T> {
    Result::map_err(Result::map(val, ResponseProxy), ResponseProxy)
}
pub fn proxy_val<T: IntoProxiedResponse>(val: T) -> Proxied<T> {
    Ok(ResponseProxy(val))
}
