pub mod api_token;
pub(crate) mod job;
pub(crate) mod perm;
pub mod supervisor;

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

#[derive(Debug, Clone, Copy, sqlx::Type)]
#[repr(transparent)]
#[sqlx(type_name = "tml_switchboard.ssh_port")]
pub struct SqlSshPort(SqlPort);

impl From<u16> for SqlSshPort {
    fn from(port: u16) -> Self {
        SqlSshPort(port.into())
    }
}

impl From<SqlSshPort> for u16 {
    fn from(sql_ssh_port: SqlSshPort) -> u16 {
        <Option<u16> as From<SqlPort>>::from(sql_ssh_port.0)
            .expect("SqlSshPort is null, must be non-null!")
    }
}

#[derive(Debug, Clone, sqlx::Type)]
#[repr(transparent)]
#[sqlx(type_name = "tml_switchboard.ssh_host")]
pub struct SqlSshHost(Option<String>);

impl From<String> for SqlSshHost {
    fn from(host: String) -> Self {
        SqlSshHost(Some(host))
    }
}

impl From<SqlSshHost> for String {
    fn from(sql_ssh_host: SqlSshHost) -> String {
        sql_ssh_host
            .0
            .expect("SqlSshHost is null, must be non-null!")
    }
}

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.ssh_endpoint")]
pub struct SqlSshEndpoint {
    pub ssh_host: SqlSshHost,
    pub ssh_port: SqlSshPort,
}

// #[derive(Debug, Clone, sqlx::Type)]
// #[sqlx(type_name = "tml_switchboard.ssh_endpoint")]
// pub struct SqlSshEndpoint {
//     pub ssh_host: String,
//     pub ssh_port: i32,
// }
