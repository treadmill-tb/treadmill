use anyhow::{anyhow, Context, Result};
use chrono::Duration;
use clap::{Args, Parser, Subcommand};
use env_logger::Builder;
use log::LevelFilter;
use log::{debug, error, info};
use reqwest::Client;
use std::collections::HashMap;
use std::env;

#[allow(unused_imports)]
use treadmill_rs::api::switchboard;
use treadmill_rs::api::switchboard::jobs::list::Response as ListJobsResponse;
use treadmill_rs::api::switchboard::jobs::status::Response as JobStatusResponse;
use treadmill_rs::api::switchboard::jobs::submit::Request as SubmitJobRequest;
use treadmill_rs::api::switchboard::{
    JobInitSpec, JobRequest, JobState, LoginRequest, LoginResponse, SupervisorStatus,
};
use treadmill_rs::api::switchboard_supervisor::ParameterValue;
use treadmill_rs::api::switchboard_supervisor::RestartPolicy;
use treadmill_rs::connector::RunningJobState;
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

use rpassword::read_password;
use std::io::{self, Write};

mod auth;
mod config;

#[derive(Parser, Debug)]
#[command(
    name = "tml",
    version = "2.0",
    author = "Treadmill Project Developers <treadmill@tockos.org>",
    about = "Treadmill Testbed CLI",
    long_about = "A command-line interface tool for interacting with the Treadmill test bench system."
)]
struct Cli {
    /// Sets a custom config file
    #[arg(short = 'c', long = "config", value_name = "FILE")]
    config: Option<String>,

    /// Sets the API URL directly
    #[arg(short = 'u', long = "api-url", value_name = "URL")]
    api_url: Option<String>,

    /// Enable verbose logging
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Login(LoginArgs),
    Job {
        #[command(subcommand)]
        job_command: JobCommands,
    },
    Supervisor {
        #[command(subcommand)]
        supervisor_command: SupervisorCommands,
    },
}

#[derive(Subcommand, Debug)]
enum JobCommands {
    /// Enqueue a new job
    Enqueue {
        /// The 64-character hex-encoded image ID
        image_id: String,

        /// Optional SSH public key to use (if not provided, one will be generated)
        #[arg(long = "ssh-key", value_name = "KEY")]
        ssh_key: Option<String>,

        /// Remaining restart count
        #[arg(long = "restart-count", value_name = "COUNT")]
        restart_count: Option<String>,

        /// JSON object of job parameters
        #[arg(long = "parameters", value_name = "PARAMS")]
        parameters: Option<String>,

        /// Tag configuration
        #[arg(long = "tag-config", value_name = "CONFIG")]
        tag_config: Option<String>,

        /// Override timeout in seconds
        #[arg(long = "timeout", value_name = "TIMEOUT")]
        timeout: Option<String>,
    },
    List,
    Status {
        /// The UUID of the job
        job_id: String,
    },
    Cancel {
        /// The UUID of the job
        job_id: String,
    },
    Ssh {
        /// The UUID of the job to connect to
        job_id: String,

        /// Optional username (defaults to "root")
        #[arg(long = "user", default_value = "root")]
        username: String,
    },
}

#[derive(Subcommand, Debug)]
enum SupervisorCommands {
    List,
    Status {
        /// The UUID of the supervisor
        supervisor_id: String,
    },
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Optional positional username
    username: Option<String>,

    /// Optional positional password
    password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        Builder::new().filter(None, LevelFilter::Debug).init();
    } else {
        Builder::new().filter(None, LevelFilter::Info).init();
    }

    // Determine config from CLI flags or XDG paths
    let config = match (cli.config.as_deref(), cli.api_url.as_deref()) {
        (Some(config_path), None) => config::load_config(Some(config_path))?,
        (None, Some(api_url)) => config::Config {
            api: config::Api {
                url: api_url.to_string(),
            },
            ssh_keys: None,
        },
        (Some(config_path), Some(api_url)) => {
            let mut cfg = config::load_config(Some(config_path))?;
            cfg.api.url = api_url.to_string();
            cfg
        }
        (None, None) => config::load_config(None)?,
    };

    let client = Client::new();

    match cli.command {
        // tml login
        Commands::Login(login_args) => {
            // Merge user & password from flags, positionals, env vars, or prompt
            let (username, password) = resolve_login_args(&login_args)?;
            log::info!("Attempting login for user: {}", username);

            login(&client, &config, &username, &password).await?;
        }

        // tml job ...
        Commands::Job { job_command } => match job_command {
            // tml job enqueue <IMAGE_ID> [--ssh-keys ...] [--restart-count ...] ...
            // tml job list
            JobCommands::Enqueue {
                image_id,
                ssh_key,
                restart_count,
                parameters,
                tag_config,
                timeout,
            } => {
                info!("Enqueueing job with image ID: {image_id}");
                enqueue_job(
                    &client,
                    &config,
                    &image_id,
                    ssh_key.as_deref(),
                    restart_count.as_deref(),
                    parameters.as_deref(),
                    tag_config.as_deref(),
                    timeout.as_deref(),
                )
                .await?;
            }
            JobCommands::List => {
                info!("Listing all jobs");
                list_jobs(&client, &config).await?;
            }
            // tml job status <JOB_ID>
            JobCommands::Status { job_id } => {
                let job_id_parsed = Uuid::parse_str(&job_id).context("Invalid job ID")?;
                info!("Getting status for job ID: {job_id_parsed}");
                get_job_status(&client, &config, job_id_parsed).await?;
            }
            // tml job cancel <JOB_ID>
            JobCommands::Cancel { job_id } => {
                let job_id_parsed = Uuid::parse_str(&job_id).context("Invalid job ID")?;
                info!("Cancelling job with ID: {job_id_parsed}");
                cancel_job(&client, &config, job_id_parsed).await?;
            }
            JobCommands::Ssh { job_id, username } => {
                info!("Connecting to job {} as user {}", job_id, username);
                ssh_into_job(&client, &config, &job_id, &username).await?;
            }
        },

        // tml supervisor ...
        Commands::Supervisor { supervisor_command } => match supervisor_command {
            // tml supervisor list
            SupervisorCommands::List => {
                info!("Listing all supervisors");
                list_supervisors(&client, &config).await?;
            }
            // tml supervisor status <SUPERVISOR_ID>
            SupervisorCommands::Status { supervisor_id } => {
                let supervisor_id_parsed =
                    Uuid::parse_str(&supervisor_id).context("Invalid supervisor ID")?;
                info!("Getting status for supervisor ID: {supervisor_id_parsed}");
                get_supervisor_status(&client, &config, supervisor_id_parsed).await?;
            }
        },
    }

    Ok(())
}

/// The order of precedence is:
/// - For username:
///   1) `login_args.username` (positional arg #1)
///   2) `TML_USER` env var
///   3) prompt
///
/// - For password:
///   1) `login_args.password` (positional arg #2)
///   2) `TML_PASSWORD` env var
///   3) prompt
pub fn resolve_login_args(login_args: &LoginArgs) -> Result<(String, String)> {
    let username = match &login_args.username {
        Some(u) => u.clone(),
        None => match env::var("TML_USER") {
            Ok(u) => u,
            Err(_) => prompt_for_input("Username")?,
        },
    };

    let password = match &login_args.password {
        Some(p) => p.clone(),
        None => match env::var("TML_PASSWORD") {
            Ok(p) => p,
            Err(_) => prompt_for_password("Password")?,
        },
    };

    Ok((username, password))
}
/// Prompts for unhidden text input (e.g., username).
fn prompt_for_input(prompt: &str) -> Result<String> {
    print!("{}: ", prompt);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Prompts for password input (hidden) using rpassword.
fn prompt_for_password(prompt: &str) -> Result<String> {
    print!("{}: ", prompt);
    io::stdout().flush()?;
    let password = read_password()?;
    Ok(password.trim().to_string())
}

//fn generate_ed25519_key(key_path: &Path) -> Result<()> {
//    // Create an RNG
//    let mut rng = OsRng;
//
//    let private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
//        .map_err(|e| anyhow!("Failed to generate Ed25519 key: {e}"))?;
//
//    // Convert to OpenSSH format
//    let openssh_private_key = private_key
//        .to_openssh(LineEnding::LF)
//        .map_err(|e| anyhow!("Failed to convert to OpenSSH format: {e}"))?;
//
//    fs::write(&key_path, openssh_private_key)?;
//    let mut perms = fs::metadata(&key_path)?.permissions();
//    perms.set_mode(0o600);
//    fs::set_permissions(&key_path, perms)?;
//
//    Ok(())
//}

async fn ssh_into_job(
    client: &Client,
    config: &config::Config,
    job_id: &str,
    username: &str,
) -> Result<()> {
    let job_id = Uuid::parse_str(job_id).context("Invalid job ID")?;

    // Get the job's SSH endpoints
    let endpoints = auth::get_job_ssh_endpoints(client, config, job_id).await?;

    if endpoints.is_empty() {
        return Err(anyhow!("No SSH endpoints available for this job"));
    }

    // Use the first available endpoint
    let endpoint = &endpoints[0];

    // Get the path to our private key
    let xdg_dirs = xdg::BaseDirectories::with_prefix("treadmill-tb")
        .context("Failed to initialize XDG base directories")?;
    let key_path = xdg_dirs.get_data_file("ssh-key");

    // Ensure the key exists
    if !key_path.exists() {
        return Err(anyhow!(
            "SSH key not found. Please enqueue a job first to generate one."
        ));
    }

    println!(
        "Connecting via SSH to {}@{} using key {:?}",
        username, endpoint, key_path
    );

    let status = std::process::Command::new("ssh")
        .arg("-i")
        .arg(&key_path)
        .arg(format!("{}@{}", username, endpoint))
        .status()
        .context("Failed to spawn SSH process")?;

    if !status.success() {
        return Err(anyhow!(
            "SSH process exited with status: {:?}",
            status.code()
        ));
    }

    Ok(())
}

async fn login(
    client: &Client,
    config: &config::Config,
    username: &str,
    password: &str,
) -> Result<()> {
    debug!("Creating login request for user: {}", username);
    let login_request = LoginRequest {
        user_identifier: username.to_string(),
        password: password.to_string(),
    };

    debug!("Sending login request to url: {}", config.api.url);

    let response = client
        .post(&format!("{}/api/v1/tokens/login", config.api.url))
        .json(&login_request)
        .send()
        .await?;

    if response.status().is_success() {
        let login_response: LoginResponse = response.json().await?;
        info!(
            "Login successful. Token expires at: {}",
            login_response.expires_at
        );
        println!(
            "Login successful. Token expires at: {}",
            login_response.expires_at
        );
        auth::save_token(&login_response.token)?;
    } else {
        let error_text = response.text().await?;
        error!("Login failed: {}", error_text);
        println!("Login failed: {}", error_text);
    }

    Ok(())
}

async fn enqueue_job(
    client: &Client,
    config: &config::Config,
    image_id: &str,
    ssh_key: Option<&str>,
    restart_count: Option<&str>,
    parameters: Option<&str>,
    tag_config: Option<&str>,
    timeout: Option<&str>,
) -> Result<()> {
    let token = auth::get_token()?;

    // Validate image_id
    let image_id_bytes = hex::decode(image_id).context("Invalid image ID")?;
    let image_id = if image_id_bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&image_id_bytes);
        ImageId(arr)
    } else {
        return Err(anyhow::anyhow!("Invalid image ID length"));
    };

    // Generate or use provided SSH key
    let ssh_keys = if let Some(key) = ssh_key {
        vec![key.to_string()]
    } else {
        let (private_key, public_key) = auth::generate_job_ssh_key()?;
        auth::save_private_key(&private_key)?;
        vec![public_key]
    };

    let restart_count = restart_count
        .map(|count| count.parse().context("Invalid restart count"))
        .transpose()?
        .unwrap_or(0);

    let parameters: HashMap<String, ParameterValue> = parameters
        .map(|params| serde_json::from_str(params).context("Invalid parameters JSON"))
        .transpose()?
        .unwrap_or_default();

    let tag_config = tag_config.unwrap_or("").to_string();

    let override_timeout: Option<Duration> = timeout
        .map(|seconds_str| -> Result<Duration> {
            let seconds = seconds_str.parse::<i64>().context("Invalid timeout")?;
            Ok(Duration::seconds(seconds))
        })
        .transpose()?;

    let job_request = JobRequest {
        init_spec: JobInitSpec::Image { image_id },
        ssh_keys,
        restart_policy: RestartPolicy {
            remaining_restart_count: restart_count,
        },
        parameters,
        tag_config,
        override_timeout,
    };

    let enqueue_request = SubmitJobRequest { job_request };

    let response = client
        .post(&format!("{}/api/v1/jobs/new", config.api.url))
        .bearer_auth(token)
        .json(&enqueue_request)
        .send()
        .await?;

    if response.status().is_success() {
        let response_json: serde_json::Value = response.json().await?;
        println!("Job enqueued successfully: {}", response_json);
    } else {
        let error_text = response.text().await?;
        return Err(anyhow!("Failed to enqueue job: {}", error_text));
    }

    Ok(())
}

async fn get_job_status(client: &Client, config: &config::Config, job_id: Uuid) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .get(&format!("{}/api/v1/jobs/{}/status", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let job_status: JobStatusResponse = response.json().await?;
        println!("Job status: {:?}", job_status);
    } else {
        let error_text = response.text().await?;
        error!("Failed to get job status: {}", error_text);
        println!("Failed to get job status: {}", error_text);
    }

    Ok(())
}

async fn cancel_job(client: &Client, config: &config::Config, job_id: Uuid) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .delete(&format!("{}/api/v1/jobs/{}", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let response_text = response.text().await?;
        println!("Job cancellation response: {}", response_text);
    } else {
        let error_text = response.text().await?;
        error!("Failed to cancel job: {}", error_text);
        println!("Failed to cancel job: {}", error_text);
    }

    Ok(())
}

async fn list_jobs(client: &Client, config: &config::Config) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .get(&format!("{}/api/v1/jobs", config.api.url))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let job_queue: ListJobsResponse = response.json().await?;
        match job_queue {
            ListJobsResponse::Ok { jobs } => {
                println!("Jobs:");
                for (index, (job_id, status)) in jobs.iter().enumerate() {
                    let desc = match &status.state.state {
                        JobState::Queued => "queued".to_string(),
                        JobState::Scheduled => format!(
                            "scheduled (supervisor={})",
                            status.state.dispatched_to_supervisor.as_ref().unwrap()
                        ),
                        JobState::Initializing { .. } => format!(
                            "starting (supervisor={})",
                            status.state.dispatched_to_supervisor.as_ref().unwrap()
                        ),
                        JobState::Ready => format!(
                            "ready (supervisor={})",
                            status.state.dispatched_to_supervisor.as_ref().unwrap()
                        ),
                        JobState::Terminating => format!(
                            "terminating (supervisor={})",
                            status.state.dispatched_to_supervisor.as_ref().unwrap()
                        ),
                        JobState::Terminated => {
                            if let Some(res) = &status.state.result {
                                format!("terminated at {}: {}", res.terminated_at, res.exit_status,)
                            } else {
                                "terminated".to_string()
                            }
                        }
                    };
                    println!("{}. {} ({})", index + 1, job_id, desc);
                }
            }
            ListJobsResponse::Internal => {
                error!("Internal server error while fetching job list");
                println!("Failed to fetch job queue due to an internal server error");
            }
            ListJobsResponse::Unauthorized => {
                error!("Unauthorized to access job list");
                println!("You are not authorized to access the job list");
            }
        }
    } else {
        let error_text = response.text().await?;
        error!("Failed to fetch job queue: {}", error_text);
        println!("Failed to fetch job queue: {}", error_text);
    }

    Ok(())
}

async fn list_supervisors(client: &Client, config: &config::Config) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .get(&format!("{}/api/v1/supervisors", config.api.url))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        use treadmill_rs::api::switchboard::supervisors::list::Response;
        let Response::Ok { supervisors } = response.json().await? else {
            unreachable!();
        };
        println!("Supervisors:");
        for (index, (supervisor_id, status)) in supervisors.iter().enumerate() {
            let desc = match status {
                SupervisorStatus::Busy { job_id, job_state } => {
                    let jstate = match job_state {
                        RunningJobState::Initializing { .. } => "starting",
                        RunningJobState::Ready => "ready",
                        RunningJobState::Terminating => "terminating",
                        RunningJobState::Terminated => "terminated (?)",
                    };
                    format!("busy (job={job_id}, {jstate})")
                }
                SupervisorStatus::BusyDisconnected { job_id, .. } => {
                    format!("busy (job={job_id}, disconnected)")
                }
                SupervisorStatus::Idle => "idle".to_string(),
                SupervisorStatus::Disconnected => "idle (disconnected)".to_string(),
            };
            println!("{}. {} ({})", index + 1, supervisor_id, desc);
        }
    } else {
        let error_text = response.text().await?;
        error!("Failed to fetch supervisor list: {}", error_text);
        println!("Failed to fetch supervisor list: {}", error_text);
    }

    Ok(())
}

async fn get_supervisor_status(
    client: &Client,
    config: &config::Config,
    supervisor_id: Uuid,
) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .get(&format!(
            "{}/api/v1/supervisors/{}/status",
            config.api.url, supervisor_id
        ))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let status: SupervisorStatus = response.json().await?;
        println!("Supervisor Status for {supervisor_id}:");
        match status {
            SupervisorStatus::Busy { job_id, job_state }
            | SupervisorStatus::BusyDisconnected { job_id, job_state } => {
                println!("  Status: Running Job");
                println!("  Job ID: {job_id}");
                println!("  Job State: {job_state:?}");
            }
            SupervisorStatus::Idle | SupervisorStatus::Disconnected => {
                println!("  Status: Idle");
            }
        }
    } else {
        let error_text = response.text().await?;
        error!("Failed to fetch supervisor status: {}", error_text);
        println!("Failed to fetch supervisor status: {}", error_text);
    }

    Ok(())
}
