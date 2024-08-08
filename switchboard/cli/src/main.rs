use anyhow::{Context, Result};
use chrono::Duration;
use clap::{App, Arg, SubCommand};
use log::{debug, error, info, warn};
use reqwest::Client;
use std::collections::HashMap;
use treadmill_rs::api::switchboard_supervisor::{ParameterValue, RendezvousServerSpec};
use uuid::Uuid;

use treadmill_rs::api::switchboard::{
    EnqueueJobRequest, JobRequest, JobStatusResponse, LoginRequest, LoginResponse,
};
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;

mod auth;
mod config;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new("Switchboard CLI")
        .version("1.0")
        .author("Benjamin Prevor")
        .about("CLI for Switchboard API")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("api_url")
                .short("u")
                .long("api-url")
                .value_name("URL")
                .help("Sets the API URL directly")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("log")
                .long("log")
                .help("Enable logging")
                .takes_value(false),
        )
        .subcommand(
            SubCommand::with_name("login")
                .about("Log in to the Switchboard API")
                .arg(Arg::with_name("username").required(true))
                .arg(Arg::with_name("password").required(true)),
        )
        .subcommand(
            SubCommand::with_name("job")
                .about("Job-related commands")
                .subcommand(
                    SubCommand::with_name("enqueue")
                        .about("Enqueue a new job")
                        .arg(Arg::with_name("image_id").required(true))
                        .arg(
                            Arg::with_name("request_id")
                                .long("request-id")
                                .value_name("REQUEST_ID")
                                .help("Request ID (UUID)")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("ssh_keys")
                                .long("ssh-keys")
                                .value_name("KEYS")
                                .help("Comma-separated list of SSH public keys")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("restart_count")
                                .long("restart-count")
                                .value_name("COUNT")
                                .help("Remaining restart count")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("rendezvous_servers")
                                .long("rendezvous-servers")
                                .value_name("SERVERS")
                                .help("JSON array of rendezvous server specifications")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("parameters")
                                .long("parameters")
                                .value_name("PARAMS")
                                .help("JSON object of job parameters")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("tag_config")
                                .long("tag-config")
                                .value_name("CONFIG")
                                .help("Tag configuration")
                                .takes_value(true),
                        )
                        .arg(
                            Arg::with_name("override_timeout")
                                .long("override-timeout")
                                .value_name("TIMEOUT")
                                .help("Override timeout in seconds")
                                .takes_value(true),
                        ),
                )
                .subcommand(SubCommand::with_name("list").about("List all jobs"))
                .subcommand(
                    SubCommand::with_name("status")
                        .about("Get job status")
                        .arg(Arg::with_name("job_id").required(true)),
                )
                .subcommand(
                    SubCommand::with_name("cancel")
                        .about("Cancel a job")
                        .arg(Arg::with_name("job_id").required(true)),
                ),
        )
        .get_matches();

    if matches.is_present("log") {
        env_logger::init();
    }

    let config = match (matches.value_of("config"), matches.value_of("api_url")) {
        (Some(config_path), None) => config::load_config(Some(config_path))?,
        (None, Some(api_url)) => config::Config {
            api: config::Api {
                url: api_url.to_string(),
            },
        },
        (Some(config_path), Some(api_url)) => {
            let mut config = config::load_config(Some(config_path))?;
            config.api.url = api_url.to_string();
            config
        }
        (None, None) => {
            return Err(anyhow::anyhow!(
                "Either a config file (-c/--config) or an API URL (-u/--api-url) must be provided"
            ));
        }
    };

    let client = Client::new();

    match matches.subcommand() {
        ("login", Some(login_matches)) => {
            let username = login_matches.value_of("username").unwrap();
            let password = login_matches.value_of("password").unwrap();
            info!("Attempting login for user: {}", username);
            login(&client, &config, username, password).await?;
        }
        ("job", Some(job_matches)) => match job_matches.subcommand() {
            ("enqueue", Some(enqueue_matches)) => {
                let image_id = enqueue_matches.value_of("image_id").unwrap();
                let request_id = enqueue_matches
                    .value_of("request_id")
                    .map(|id| Uuid::parse_str(id).context("Invalid request ID"))
                    .transpose()?;
                let ssh_keys = enqueue_matches.value_of("ssh_keys");
                let restart_count = enqueue_matches.value_of("restart_count");
                let rendezvous_servers = enqueue_matches.value_of("rendezvous_servers");
                let parameters = enqueue_matches.value_of("parameters");
                let tag_config = enqueue_matches.value_of("tag_config");
                let override_timeout = enqueue_matches.value_of("override_timeout");

                info!("Enqueueing job with image ID: {}", image_id);
                enqueue_job(
                    &client,
                    &config,
                    request_id,
                    image_id,
                    ssh_keys,
                    restart_count,
                    rendezvous_servers,
                    parameters,
                    tag_config,
                    override_timeout,
                )
                .await?;
            }
            ("list", Some(_)) => {
                warn!("Job list functionality not implemented yet");
                println!("Job list functionality not implemented yet");
            }
            ("status", Some(status_matches)) => {
                let job_id = Uuid::parse_str(status_matches.value_of("job_id").unwrap())
                    .context("Invalid job ID")?;
                info!("Getting status for job ID: {}", job_id);
                get_job_status(&client, &config, job_id).await?;
            }
            ("cancel", Some(cancel_matches)) => {
                let job_id = Uuid::parse_str(cancel_matches.value_of("job_id").unwrap())
                    .context("Invalid job ID")?;
                info!("Cancelling job with ID: {}", job_id);
                cancel_job(&client, &config, job_id).await?;
            }
            _ => {
                error!("Invalid job subcommand");
                println!("Invalid job subcommand");
            }
        },
        _ => {
            error!("Invalid command");
            println!("Invalid command");
        }
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

    let response: LoginResponse = client
        .post(&format!("{}/session/login", config.api.url))
        .json(&login_request)
        .send()
        .await?
        .json()
        .await?;

    info!(
        "Login successful. Token expires at: {}",
        response.expires_at
    );
    println!(
        "Login successful. Token expires at: {}",
        response.expires_at
    );
    auth::save_token(&response.token)?;

    Ok(())
}

async fn enqueue_job(
    client: &Client,
    config: &config::Config,
    request_id: Option<Uuid>,
    image_id: &str,
    ssh_keys: Option<&str>,
    restart_count: Option<&str>,
    rendezvous_servers: Option<&str>,
    parameters: Option<&str>,
    tag_config: Option<&str>,
    override_timeout: Option<&str>,
) -> Result<()> {
    let token = auth::get_token()?;
    debug!("Retrieved auth token");

    let image_id_bytes = hex::decode(image_id).context("Invalid image ID")?;
    let image_id = if image_id_bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&image_id_bytes);
        ImageId(arr)
    } else {
        error!("Invalid image ID length");
        return Err(anyhow::anyhow!("Invalid image ID length"));
    };

    let ssh_keys = ssh_keys
        .map(|keys| keys.split(',').map(String::from).collect())
        .unwrap_or_default();

    let restart_count = restart_count
        .map(|count| count.parse().context("Invalid restart count"))
        .transpose()?
        .unwrap_or(0);

    let rendezvous_servers: Vec<RendezvousServerSpec> = rendezvous_servers
        .map(|servers| serde_json::from_str(servers).context("Invalid rendezvous servers JSON"))
        .transpose()?
        .unwrap_or_default();

    let parameters: HashMap<String, ParameterValue> = parameters
        .map(|params| serde_json::from_str(params).context("Invalid parameters JSON"))
        .transpose()?
        .unwrap_or_default();

    let tag_config = tag_config.unwrap_or("").to_string();

    let override_timeout: Option<Duration> = override_timeout
        .map(|timeout| -> Result<Duration> {
            let seconds = timeout.parse::<i64>().context("Invalid timeout")?;
            Ok(Duration::seconds(seconds))
        })
        .transpose()?;

    debug!("Creating job request");
    let job_request = JobRequest {
        init_spec: JobInitSpec::Image { image_id },
        ssh_keys,
        restart_policy: RestartPolicy {
            remaining_restart_count: restart_count,
        },
        ssh_rendezvous_servers: rendezvous_servers,
        parameters,
        tag_config,
        override_timeout,
    };

    let enqueue_request = EnqueueJobRequest { job_request };

    debug!("Sending enqueue job request");
    let response = client
        .post(&format!("{}/api/v1/job/queue", config.api.url))
        .bearer_auth(token)
        .header(
            "X-Request-ID",
            request_id.unwrap_or_else(Uuid::new_v4).to_string(),
        )
        .json(&enqueue_request)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    if let Some(error_type) = response.get("type") {
        if error_type == "Internal" {
            error!("Internal server error occurred: {:?}", response);
            return Err(anyhow::anyhow!("Internal server error: {:?}", response));
        }
    }

    info!("Job enqueued: {}", response);
    println!("Job enqueued: {}", response);
    Ok(())
}

async fn get_job_status(client: &Client, config: &config::Config, job_id: Uuid) -> Result<()> {
    let token = auth::get_token()?;

    let response: JobStatusResponse = client
        .get(&format!("{}/api/v1/job/{}/status", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?
        .json()
        .await?;

    println!("Job status: {:?}", response);
    Ok(())
}

async fn cancel_job(client: &Client, config: &config::Config, job_id: Uuid) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .delete(&format!("{}/api/v1/job/{}", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?
        .text()
        .await?;

    println!("Job cancellation response: {}", response);
    Ok(())
}
