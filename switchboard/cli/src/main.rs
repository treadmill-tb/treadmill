use anyhow::{Context, Result};
use chrono::Duration;
use clap::{App, Arg, SubCommand};
use env_logger::Builder;
use log::LevelFilter;
use log::{debug, error, info, warn};
use reqwest::Client;
use std::collections::HashMap;
use treadmill_rs::api::switchboard_supervisor::ParameterValue;
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
            Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .help("Enable verbose logging")
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
                        .arg(Arg::with_name("ssh_keys")
                                .long("ssh-keys")
                                .value_name("KEYS")
                                .help("Comma-separated list of SSH public keys. If not provided, keys will be read from SSH agent, public key files, and config file.")
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
                            Arg::with_name("timeout")
                                .long("timeout")
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
        .setting(clap::AppSettings::SubcommandRequiredElseHelp)
        .get_matches();

    if matches.is_present("verbose") {
        Builder::new().filter(None, LevelFilter::Debug).init();
    } else {
        Builder::new().filter(None, LevelFilter::Info).init();
    }

    let config = match (matches.value_of("config"), matches.value_of("api_url")) {
        (Some(config_path), None) => config::load_config(Some(config_path))?,
        (None, Some(api_url)) => config::Config {
            api: config::Api {
                url: api_url.to_string(),
            },
            ssh_keys: None,
        },
        (Some(config_path), Some(api_url)) => {
            let mut config = config::load_config(Some(config_path))?;
            config.api.url = api_url.to_string();
            config
        }
        (None, None) => config::load_config(None)?,
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
                let ssh_keys = enqueue_matches.value_of("ssh_keys");
                let restart_count = enqueue_matches.value_of("restart_count");
                let parameters = enqueue_matches.value_of("parameters");
                let tag_config = enqueue_matches.value_of("tag_config");
                let timeout = enqueue_matches.value_of("timeout");

                info!("Enqueueing job with image ID: {}", image_id);
                enqueue_job(
                    &client,
                    &config,
                    image_id,
                    ssh_keys,
                    restart_count,
                    parameters,
                    tag_config,
                    timeout,
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
                if let Some(job_subcommand) = job_matches.subcommand_name() {
                    if let Some(subcommand_matches) = job_matches.subcommand_matches(job_subcommand)
                    {
                        subcommand_matches.usage();
                    }
                } else {
                    job_matches.usage();
                }
            }
        },
        _ => {
            error!("Invalid command");
            println!("Invalid command");
            matches.usage();
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

    let response = client
        .post(&format!("{}/session/login", config.api.url))
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
    ssh_keys: Option<&str>,
    restart_count: Option<&str>,
    parameters: Option<&str>,
    tag_config: Option<&str>,
    timeout: Option<&str>,
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

    let ssh_keys = if let Some(keys) = ssh_keys {
        keys.split(',').map(String::from).collect()
    } else {
        auth::read_ssh_keys()?
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
        ssh_rendezvous_servers: Vec::new(),
        parameters,
        tag_config,
        override_timeout,
    };

    let enqueue_request = EnqueueJobRequest { job_request };

    debug!("Sending enqueue job request");
    let response = client
        .post(&format!("{}/api/v1/job/queue", config.api.url))
        .bearer_auth(token)
        .json(&enqueue_request)
        .send()
        .await?;

    if response.status().is_success() {
        let response_json: serde_json::Value = response.json().await?;
        info!("Job enqueued: {}", response_json);
        println!("Job enqueued: {}", response_json);
    } else {
        let error_text = response.text().await?;
        error!("Failed to enqueue job: {}", error_text);
        println!("Failed to enqueue job: {}", error_text);
    }

    Ok(())
}

async fn get_job_status(client: &Client, config: &config::Config, job_id: Uuid) -> Result<()> {
    let token = auth::get_token()?;

    let response = client
        .get(&format!("{}/api/v1/job/{}/status", config.api.url, job_id))
        .bearer_auth(token)
        .send()
        .await?;

    if response.status().is_success() {
        let job_status: JobStatusResponse = response.json().await?;
        println!("Job status: {:?}", job_status);
        // TODO: job state history display here when available from the API
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
        .delete(&format!("{}/api/v1/job/{}", config.api.url, job_id))
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
