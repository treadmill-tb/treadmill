pub mod api_token;
pub mod host;
pub mod host_spec;
pub mod image;
pub(crate) mod job;
pub mod oauth_flow;
pub mod staged_login;
pub mod user;

#[derive(Debug, Clone, Copy, sqlx::Type)]
#[repr(transparent)]
#[sqlx(type_name = "tml_switchboard.port")]
pub struct SqlPort(Option<i32>);

impl From<Option<u16>> for SqlPort {
    fn from(opt_port: Option<u16>) -> Self {
        SqlPort(opt_port.map(|p| p as i32))
    }
}

impl From<u16> for SqlPort {
    fn from(port: u16) -> Self {
        Some(port).into()
    }
}

impl From<SqlPort> for Option<u16> {
    fn from(sql_port: SqlPort) -> Option<u16> {
        sql_port
            .0
            .map(|i32_port| i32_port.try_into().expect("SqlPort out of range!"))
    }
}
