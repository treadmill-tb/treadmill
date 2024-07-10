use crate::server::AppState;
use axum::routing::post;
use axum::Router;

mod csrf;
pub mod session;

pub fn build_router() -> Router<AppState> {
    Router::new()
        .route("/session/presession", post(session::presession_handler))
        .route("/session/login", post(session::login_hapub   fn build_router() -> Router<AppState>  {
    todo!()
}pub   fn build_router() -> Router<AppState>  {
    todo!()
}pub   fn build_router() -> Router<AppState>  {
    todo!()
}ndler))
}
