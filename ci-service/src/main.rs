use std::time::{SystemTime, UNIX_EPOCH};

use fastly::http::StatusCode;
use fastly::{ConfigStore, Error, Request, Response, SecretStore};
use jsonwebtoken::EncodingKey;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct GitHubJwtPayload {
    // issued at time, 60 seconds in the past to allow for clock drift
    iat: u64, // epoch seconds
    // JWT expiration time (10 minute maximum)
    exp: u64, // epoch seconds
    // GitHub App's client ID
    iss: String,
}

#[fastly::main]
fn main(_req: Request) -> Result<Response, Error> {
    let secret_store = SecretStore::open("github")?;
    let config_store = ConfigStore::open("github");

    let private_key = secret_store
        .get("privatekey")
        .expect("private key")
        .plaintext();
    let client_id = config_store.get("clientid").expect("client_id");

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let my_claims = GitHubJwtPayload {
        iat: now - 60,
        exp: now + 300,
        iss: client_id,
    };

    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &my_claims,
        &EncodingKey::from_rsa_pem(&private_key)?,
    )?;

    let resp = Request::get("https://api.github.com/app")
        .with_header("Authorization", format!("Bearer {token}"))
        .with_header("Accept", "application/vnd.github+json")
        .with_header("X-GitHub-Api-Version", "2026-03-10")
        .with_header("User-Agent", "treadmill")
        .send("githubapi")?;

    #[derive(Deserialize)]
    struct GHAppResponse {
        name: String,
    }
    let app: GHAppResponse = serde_json::from_reader(resp.into_body())?;
    Ok(Response::from_status(StatusCode::OK)
        .with_body_text_html(&format!("<h1>Hi, my name is <em>{}</em></h1>", app.name)))
}
