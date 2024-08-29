pub mod list {
    use crate::api::switchboard::SupervisorStatus;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use tml_switchboard_macros::HttpStatusCode;
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
    //  access_supervisor_status:<ID>
    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok {
            supervisors: HashMap<Uuid, SupervisorStatus>,
        },
        /// User lacks the `list_supervisors` permission.
        #[http(status = 403)]
        Unauthorized,
        #[http(status = 500)]
        Internal,
    }
}

pub mod status {
    use crate::api::switchboard::SupervisorStatus;
    use serde::{Deserialize, Serialize};
    use tml_switchboard_macros::HttpStatusCode;

    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok { status: SupervisorStatus },
        /// User does not have access to a supervisor with that ID.
        #[http(status = 404)]
        Invalid,
        #[http(status = 500)]
        Internal,
    }
}
