pub mod list {
    use crate::api::switchboard::SupervisorStatus;
    use http::StatusCode;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use uuid::Uuid;

    #[derive(Serialize, Deserialize, Debug, Copy, Clone)]
    #[serde(rename_all = "snake_case")]
    pub enum WorkFilter {
        Idle,
        Busy,
    }
    #[derive(Serialize, Deserialize, Debug, Copy, Clone)]
    #[serde(rename_all = "snake_case")]
    pub enum ConnFilter {
        Connected,
        Disconnected,
    }
    /// Describes the query portion of the HTTP request.
    #[derive(Serialize, Deserialize, Debug, Copy, Clone)]
    pub struct Filter {
        pub work: Option<WorkFilter>,
        pub conn: Option<ConnFilter>,
    }

    // Governing Permissions:
    //  list_supervisors
    //  read_supervisor_status:<ID>
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok {
            supervisors: HashMap<Uuid, SupervisorStatus>,
        },
        /// User lacks the `list_supervisors` permission.
        Unauthorized,
        Internal,
    }

    impl crate::api::switchboard::JsonProxiedStatus for Response {
        fn status_code(&self) -> StatusCode {
            match self {
                Response::Ok { .. } => StatusCode::OK,
                Response::Unauthorized => StatusCode::FORBIDDEN,
                Response::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
    }
}

pub mod status {
    use crate::api::switchboard::SupervisorStatus;
    use http::StatusCode;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok {
            status: SupervisorStatus,
        },
        /// User does not have access to a supervisor with that ID.
        Invalid,
        Internal,
    }

    impl crate::api::switchboard::JsonProxiedStatus for Response {
        fn status_code(&self) -> StatusCode {
            match self {
                Response::Ok { .. } => StatusCode::OK,
                Response::Invalid => StatusCode::NOT_FOUND,
                Response::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
    }
}
