pub mod submit {
    use crate::api::switchboard::JobRequest;
    use serde::{Deserialize, Serialize};
    use tml_switchboard_macros::HttpStatusCode;
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Request {
        /// Job request.
        #[serde(flatten)]
        pub job_request: JobRequest,
    }

    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok { job_id: Uuid },
        /// No supervisors currently registered can carry out the request.
        #[http(status = 404)]
        SupervisorMatchError,
        /// User does not have `submit_job` permission.
        #[http(status = 403)]
        Unauthorized,
        // /// Job request is invalid for some reason.
        // #[http(status = 400)]
        // Invalid { reason: String },
        #[http(status = 500)]
        Internal,
    }
}

pub mod status {
    use crate::api::switchboard::JobStatus;
    use serde::{Deserialize, Serialize};
    use tml_switchboard_macros::HttpStatusCode;

    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok { job_status: JobStatus },
        #[http(status = 404)]
        Invalid,
        #[http(status = 500)]
        Internal,
    }
}

pub mod stop {
    use serde::{Deserialize, Serialize};
    use tml_switchboard_macros::HttpStatusCode;

    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok,
        #[http(status = 404)]
        Invalid,
        #[http(status = 500)]
        Internal,
    }
}

pub mod list {
    use crate::api::switchboard::JobStatus;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use tml_switchboard_macros::HttpStatusCode;
    use uuid::Uuid;

    // #[derive(Serialize, Deserialize, Debug, Copy, Clone)]
    // #[serde(rename_all = "snake_case")]
    // pub enum Filter {
    //     Ongoing,
    //     Running,
    //     Queued,
    //     Finished,
    // }

    // Governing Permissions:
    //  list_jobs
    //  read_job_status:<ID>
    #[derive(Debug, Clone, HttpStatusCode, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        #[http(status = 200)]
        Ok { jobs: HashMap<Uuid, JobStatus> },
        /// User lacks the `list_jobs` permission.
        #[http(status = 403)]
        Unauthorized,
        #[http(status = 500)]
        Internal,
    }
}
