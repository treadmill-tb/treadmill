// File: connector/cli/tests/cli_connector_tests.rs

use std::sync::Arc;
use tml_cli_connector::{CliConnector, CliConnectorConfig};
use treadmill_rs::connector::{
    JobError, JobState, StartJobMessage, StopJobMessage, Supervisor, SupervisorConnector,
};
use uuid::Uuid;

struct MockSupervisor;

#[async_trait::async_trait]
impl Supervisor for MockSupervisor {
    async fn start_job(_this: &Arc<Self>, _request: StartJobMessage) -> Result<(), JobError> {
        Ok(())
    }

    async fn stop_job(_this: &Arc<Self>, _request: StopJobMessage) -> Result<(), JobError> {
        Ok(())
    }
}

#[tokio::test]
async fn test_cli_connector_job_operations() {
    let config = CliConnectorConfig {
        images: Default::default(),
    };
    let supervisor = Arc::new(MockSupervisor);
    let connector = CliConnector::new(Uuid::new_v4(), config, Arc::downgrade(&supervisor));

    // Test update_job_state
    let job_id = Uuid::new_v4();
    let job_state = JobState::Starting {
        stage: treadmill_rs::connector::JobStartingStage::Allocating,
        status_message: Some("Allocating resources".to_string()),
    };
    connector.update_job_state(job_id, job_state.clone()).await;

    let error = JobError {
        error_kind: treadmill_rs::connector::JobErrorKind::JobNotFound,
        description: "Test error".to_string(),
    };
    connector.report_job_error(job_id, error).await;

    let console_bytes = b"Test console output".to_vec();
    connector.send_job_console_log(job_id, console_bytes).await;
}

#[tokio::test]
#[ignore] // This test would block indefinitely, so it is ignored
async fn test_cli_connector_run() {
    let config = CliConnectorConfig {
        images: Default::default(),
    };
    let supervisor = Arc::new(MockSupervisor);
    let connector = CliConnector::new(Uuid::new_v4(), config, Arc::downgrade(&supervisor));

    // In a real test environment, you might want to set up a way to inject commands
    // and check the results, but for now we'll just ensure it can be called
    connector.run().await;
}
