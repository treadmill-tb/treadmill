//! End-to-end tests for bot users: creation through `POST /users`, admin-minted
//! (optionally non-expiring) tokens through `POST /users/{id}/tokens`, and the
//! generalized self-or-admin token listing/revocation routes.
//!
//! Callers authenticate via the development mock-OAuth provider: `alice` is a
//! global admin, `bob` an ordinary user.

use std::net::SocketAddr;

use reqwest::StatusCode;
use reqwest::redirect::Policy;
use sqlx::PgPool;
use uuid::Uuid;

use treadmill_rs::api::switchboard::WhoAmIResponse;
use treadmill_rs::api::switchboard::images::ImageSetInfo;
use treadmill_rs::api::switchboard::users::{IssuedToken, PublicUserProfile, SessionInfo};
use treadmill_switchboard::serve::AppState;

mod common;
use common::{mock_login_token, spawn_server, test_config_mock};

struct Env {
    client: reqwest::Client,
    base: String,
    alice: String,
    bob: String,
}

async fn setup(pool: &PgPool) -> Env {
    let addr: SocketAddr = spawn_server(AppState::new(pool.clone(), test_config_mock())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let alice = mock_login_token(pool, &client, addr, "alice", true).await;
    let bob = mock_login_token(pool, &client, addr, "bob", true).await;
    Env {
        client,
        base: format!("http://{addr}/api/v1"),
        alice,
        bob,
    }
}

impl Env {
    async fn create_bot(&self, token: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(format!("{}/users", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn mint(&self, token: &str, user: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(format!("{}/users/{user}/tokens", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, token: &str, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn delete(&self, token: &str, path: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn whoami(&self, token: &str) -> WhoAmIResponse {
        let resp = self.get(token, "/auth/whoami").await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.unwrap()
    }

    /// Create a bot as alice and mint it a non-expiring token.
    async fn bot_with_token(&self) -> (PublicUserProfile, IssuedToken) {
        let resp = self
            .create_bot(
                &self.alice,
                serde_json::json!({ "name": "image builder", "bot": true }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bot: PublicUserProfile = resp.json().await.unwrap();
        let resp = self
            .mint(
                &self.alice,
                &bot.user_id.to_string(),
                serde_json::json!({ "comment": "GitHub Actions" }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        (bot, resp.json().await.unwrap())
    }
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn creating_users_requires_admin_and_bot(pool: PgPool) {
    let env = setup(&pool).await;

    let resp = env
        .create_bot(
            &env.bob,
            serde_json::json!({ "name": "rogue", "bot": true }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Humans come from interactive login only.
    let resp = env
        .create_bot(
            &env.alice,
            serde_json::json!({ "name": "human", "bot": false }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // `bot` is mandatory.
    let resp = env
        .create_bot(&env.alice, serde_json::json!({ "name": "implicit" }))
        .await;
    assert!(resp.status().is_client_error());

    let resp = env
        .create_bot(&env.alice, serde_json::json!({ "name": "  ", "bot": true }))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = env
        .create_bot(
            &env.alice,
            serde_json::json!({ "name": " builder ", "bot": true }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bot: PublicUserProfile = resp.json().await.unwrap();
    assert!(bot.bot);
    assert_eq!(bot.name, "builder");

    // The public profile reports the flag to anyone; humans are not bots.
    let profile: PublicUserProfile = env
        .get(&env.bob, &format!("/users/{}", bot.user_id))
        .await
        .json()
        .await
        .unwrap();
    assert!(profile.bot);
    assert!(!env.whoami(&env.bob).await.bot);
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn bot_tokens_authenticate_and_never_expire(pool: PgPool) {
    let env = setup(&pool).await;
    let (bot, issued) = env.bot_with_token().await;
    assert_eq!(issued.expires_at, None);

    let token = issued.token.encode_for_http();
    let who = env.whoami(&token).await;
    assert_eq!(who.user_id, bot.user_id);
    assert!(who.bot);
    assert!(!who.admin);

    // A bot is an ordinary user otherwise: it owns what it creates, and
    // `SubjectRef`s to it carry the flag.
    let resp = env
        .client
        .post(format!("{}/image-sets", env.base))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "display_name": "bot-built" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let set: ImageSetInfo = resp.json().await.unwrap();
    let owner = set.owner.expect("the bot owns its set");
    assert_eq!(owner.id, bot.user_id);
    assert!(owner.bot);

    // The bot sees its own token (as current) under `me`.
    let sessions: Vec<SessionInfo> = env
        .get(&token, "/users/me/tokens")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].current);
    assert_eq!(sessions[0].expires_at, None);
    assert_eq!(sessions[0].comment.as_deref(), Some("GitHub Actions"));

    // An expiring bot token is also possible.
    let expires_at = chrono::Utc::now() + chrono::TimeDelta::days(90);
    let resp = env
        .mint(
            &env.alice,
            &bot.user_id.to_string(),
            serde_json::json!({ "expires_at": expires_at }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let expiring: IssuedToken = resp.json().await.unwrap();
    assert!(expiring.expires_at.is_some());
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn minting_is_admin_only_and_bot_only(pool: PgPool) {
    let env = setup(&pool).await;
    let (bot, _) = env.bot_with_token().await;
    let bot_id = bot.user_id.to_string();
    let bob_id = env.whoami(&env.bob).await.user_id.to_string();

    // Not an admin: forbidden, even for a bot.
    let resp = env.mint(&env.bob, &bot_id, serde_json::json!({})).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Never for a human, not even oneself.
    let resp = env.mint(&env.alice, &bob_id, serde_json::json!({})).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let resp = env.mint(&env.alice, "me", serde_json::json!({})).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let resp = env
        .mint(
            &env.alice,
            &Uuid::now_v7().to_string(),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let past = chrono::Utc::now() - chrono::TimeDelta::minutes(1);
    let resp = env
        .mint(
            &env.alice,
            &bot_id,
            serde_json::json!({ "expires_at": past }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = env
        .mint(
            &env.alice,
            &bot_id,
            serde_json::json!({ "comment": "a\nb" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn admins_list_and_revoke_any_users_tokens(pool: PgPool) {
    let env = setup(&pool).await;
    let (bot, issued) = env.bot_with_token().await;
    let bot_token = issued.token.encode_for_http();
    let bob_id = env.whoami(&env.bob).await.user_id;

    // Bob may not see another user's tokens; that is indistinguishable from
    // the user not existing.
    let resp = env
        .get(&env.bob, &format!("/users/{}/tokens", bot.user_id))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = env
        .delete(
            &env.bob,
            &format!("/users/{}/tokens/{}", bot.user_id, issued.token_id),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = env
        .get(&env.alice, &format!("/users/{}/tokens", Uuid::now_v7()))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // An admin lists a human's sessions; none is the admin's current one.
    let sessions: Vec<SessionInfo> = env
        .get(&env.alice, &format!("/users/{bob_id}/tokens"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].current);
    assert!(sessions[0].expires_at.is_some());

    // A token id under the wrong user is not found.
    let resp = env
        .delete(
            &env.alice,
            &format!("/users/{bob_id}/tokens/{}", issued.token_id),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Revoking the bot's token voids it.
    let resp = env
        .delete(
            &env.alice,
            &format!("/users/{}/tokens/{}", bot.user_id, issued.token_id),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = env.get(&bot_token, "/auth/whoami").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And a human's session.
    let resp = env
        .delete(
            &env.alice,
            &format!("/users/{bob_id}/tokens/{}", sessions[0].token_id),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = env.get(&env.bob, "/auth/whoami").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn only_bots_may_hold_non_expiring_tokens(pool: PgPool) {
    let env = setup(&pool).await;
    let bob_id = env.whoami(&env.bob).await.user_id;

    // The database refuses a non-expiring token for a human, whatever the
    // route layer does.
    let res = sqlx::query(
        "insert into tml_switchboard.api_tokens \
         (token_id, token, subject_id, subject_kind, created_at, expires_at) \
         values ($1, $2, $3, 'user', now(), null)",
    )
    .bind(Uuid::now_v7())
    .bind(vec![7u8; 32])
    .bind(bob_id)
    .execute(&pool)
    .await;
    let err = res.expect_err("a human's token must expire");
    assert!(
        err.to_string()
            .contains("only a bot user may hold a non-expiring token"),
        "{err}"
    );
}
