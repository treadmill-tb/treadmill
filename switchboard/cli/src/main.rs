use anyhow::{Context, Result};
use clap::{App, Arg, SubCommand};
use reqwest::Client;
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, RestartPolicy};
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

use treadmill_rs::api::switchboard::{
    EnqueueJobRequest, JobRequest, JobStatusResponse, LoginRequest, LoginResponse,
};

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
                        .arg(Arg::with_name("supervisor_id").required(true))
                        .arg(Arg::with_name("image_id").required(true)),
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
            login(&client, &config, username, password).await?;
        }
        ("job", Some(job_matches)) => match job_matches.subcommand() {
            ("enqueue", Some(enqueue_matches)) => {
                let supervisor_id =
                    Uuid::parse_str(enqueue_matches.value_of("supervisor_id").unwrap())
                        .context("Invalid supervisor ID")?;
                let image_id = enqueue_matches.value_of("image_id").unwrap();
                enqueue_job(&client, &config, supervisor_id, image_id).await?;
            }
            ("list", Some(_)) => {
                println!("Job list functionality not implemented yet");
            }
            ("status", Some(status_matches)) => {
                let job_id = Uuid::parse_str(status_matches.value_of("job_id").unwrap())
                    .context("Invalid job ID")?;
                get_job_status(&client, &config, job_id).await?;
            }
            ("cancel", Some(cancel_matches)) => {
                let job_id = Uuid::parse_str(cancel_matches.value_of("job_id").unwrap())
                    .context("Invalid job ID")?;
                cancel_job(&client, &config, job_id).await?;
            }
            _ => println!("Invalid job subcommand"),
        },
        _ => println!("Invalid command"),
    }

    Ok(())
}

async fn login(
    client: &Client,
    config: &config::Config,
    username: &str,
    password: &str,
) -> Result<()> {
    let login_request = LoginRequest {
        user_identifier: username.to_string(),
        password: password.to_string(),
    };

    let response: LoginResponse = client
        .post(&format!("{}/session/login", config.api.url))
        .json(&login_request)
        .send()
        .await?
        .json()
        .await?;

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
    supervisor_id: Uuid,
    image_id: &str,
) -> Result<()> {
    let token = auth::get_token()?;

    let image_id_bytes = hex::decode(image_id).context("Invalid image ID")?;
    let image_id = if image_id_bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&image_id_bytes);
        ImageId(arr)
    } else {
        return Err(anyhow::anyhow!("Invalid image ID length"));
    };

    let job_request = JobRequest {
        init_spec: JobInitSpec::Image { image_id },
        ssh_keys: vec![],
        restart_policy: RestartPolicy {
            remaining_restart_count: 0,
        },
        ssh_rendezvous_servers: vec![],
        parameters: Default::default(),
        tag_config: String::new(),
        override_timeout: None,
    };

    let enqueue_request = EnqueueJobRequest {
        supervisor_id,
        job_request,
    };

    let response = client
        .post(&format!("{}/api/v1/job/queue", config.api.url))
        .bearer_auth(token)
        .json(&enqueue_request)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

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
