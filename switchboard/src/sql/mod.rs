pub mod api_token;
pub(crate) mod job;
pub(crate) mod perm;
pub mod supervisor;

macro_rules! sql_uuid_type {
    ($ty_name:ident, $nullable_ty_name:ident) => {
	#[derive(
            Clone,
            Copy,
            Debug,
            Eq,
            Ord,
            PartialEq,
            PartialOrd,
            serde::Deserialize,
            serde::Serialize,
	    sqlx::Type,
	)]
	#[serde(transparent)]
	#[sqlx(transparent)]
	pub struct $ty_name(pub ::uuid::Uuid);

	// impl ::sqlx::Type<::sqlx::Postgres> for $ty_name {
	//     fn type_info() -> <::sqlx::Postgres as ::sqlx::Database>::TypeInfo {
	// 	<::uuid::Uuid as ::sqlx::Type<::sqlx::Postgres>>::type_info()
	//     }
	// }

	impl ::core::convert::From<::uuid::Uuid> for $ty_name {
	    fn from(other: ::uuid::Uuid) -> Self {
		$ty_name(other)
	    }
	}

	impl ::core::convert::From<$ty_name> for ::uuid::Uuid {
	    fn from(other: $ty_name) -> Self {
		other.0
	    }
	}

	impl ::std::fmt::Display for $ty_name {
	    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::result::Result<(), ::std::fmt::Error> {
		self.0.fmt(f)
	    }
	}

	// #[derive(Debug, Copy, Clone, PartialEq, Eq, sqlx::Encode, sqlx::Decode)]
	// #[repr(transparent)]
	// pub struct $nullable_ty_name(pub std::option::Option<$ty_name>);

	// impl $nullable_ty_name {
	//     pub fn into_option(self) -> std::option::Option<$ty_name> {
	// 	self.into()
	//     }
	// }

	// impl ::sqlx::Type<::sqlx::Postgres> for $nullable_ty_name {
	//     fn type_info() -> <::sqlx::Postgres as ::sqlx::Database>::TypeInfo {
	// 	<::std::option::Option<$ty_name> as ::sqlx::Type<::sqlx::Postgres>>::type_info()
	//     }
	// }

	// impl ::core::convert::From<std::option::Option<::uuid::Uuid>> for $nullable_ty_name {
	//     fn from(other: std::option::Option<::uuid::Uuid>) -> Self {
	// 	$nullable_ty_name(other.map(|v| v.into()))
	//     }
	// }

	// impl ::core::convert::From<$nullable_ty_name> for std::option::Option<::uuid::Uuid> {
	//     fn from(other: $nullable_ty_name) -> Self {
	// 	other.0.map(|v| v.into())
	//     }
	// }

	// impl ::core::convert::From<std::option::Option<$ty_name>> for $nullable_ty_name {
	//     fn from(other: std::option::Option<$ty_name>) -> Self {
	// 	$nullable_ty_name(other)
	//     }
	// }

	// impl ::core::convert::From<$nullable_ty_name> for std::option::Option<$ty_name> {
	//     fn from(other: $nullable_ty_name) -> Self {
	// 	other.0
	//     }
	// }
    }
}

// Make macro accessible through module path:
pub(crate) use sql_uuid_type;

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
