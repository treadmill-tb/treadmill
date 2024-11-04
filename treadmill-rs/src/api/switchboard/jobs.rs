pub mod submit {
    use crate::api::switchboard::JobRequest;
    use http::StatusCode;
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Request {
        /// Job request.
        #[serde(flatten)]
        pub job_request: JobRequest,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok {
            job_id: Uuid,
        },
        /// No supervisors currently registered can carry out the request.
        SupervisorMatchError,
        /// User does not have `submit_job` permission.
        Unauthorized,
        // /// Job request is invalid for some reason.
        // Invalid { reason: String },
        Internal,
    }

    impl crate::api::switchboard::JsonProxiedStatus for Response {
        fn status_code(&self) -> StatusCode {
            match self {
                Response::Ok { .. } => StatusCode::OK,
                Response::SupervisorMatchError => StatusCode::NOT_FOUND,
                Response::Unauthorized => StatusCode::FORBIDDEN,
                Response::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
    }
}

pub mod status {
    use crate::api::switchboard::JobStatus;
    use http::StatusCode;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok { job_status: JobStatus },
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

pub mod stop {
    use http::StatusCode;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok,
        Invalid,
        Internal,
    }

    impl crate::api::switchboard::JsonProxiedStatus for Response {
        fn status_code(&self) -> StatusCode {
            match self {
                Response::Ok => StatusCode::OK,
                Response::Invalid => StatusCode::NOT_FOUND,
                Response::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
    }
}

pub mod list {
    use crate::api::switchboard::JobStatus;
    use http::StatusCode;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
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
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok {
            jobs: HashMap<Uuid, JobStatus>,
        },
        /// User lacks the `list_jobs` permission.
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
