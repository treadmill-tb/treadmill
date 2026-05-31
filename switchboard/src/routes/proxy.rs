use aide::{OperationInput, OperationOutput};
use axum::extract;
use axum::response::{IntoResponse, Response};
use std::borrow::Cow;
use std::fmt::{Debug, Formatter};
pub use treadmill_rs::api::switchboard::JsonProxiedStatus;

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

impl<T: IntoProxiedResponse + schemars::JsonSchema> schemars::JsonSchema for ResponseProxy<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }
    fn json_schema(r#gen: &mut schemars::generate::SchemaGenerator) -> schemars::Schema {
        T::json_schema(r#gen)
    }
}

impl<T: IntoProxiedResponse + schemars::JsonSchema + 'static> aide::OperationOutput
    for ResponseProxy<T>
{
    type Inner = T;
    fn operation_response(
        ctx: &mut aide::generate::GenContext,
        op: &mut aide::openapi::Operation,
    ) -> Option<aide::openapi::Response> {
        axum::Json::<T>::operation_response(ctx, op)
    }
    fn inferred_responses(
        ctx: &mut aide::generate::GenContext,
        op: &mut aide::openapi::Operation,
    ) -> Vec<(Option<aide::openapi::StatusCode>, aide::openapi::Response)> {
        axum::Json::<T>::inferred_responses(ctx, op)
    }
}

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
#[allow(dead_code)]
pub fn proxy<T: IntoProxiedResponse>(val: Result<T, T>) -> Proxied<T> {
    Result::map_err(Result::map(val, ResponseProxy), ResponseProxy)
}
pub fn proxy_val<T: IntoProxiedResponse>(val: T) -> Proxied<T> {
    Ok(ResponseProxy(val))
}
