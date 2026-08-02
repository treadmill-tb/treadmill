//! Admin management commands for the Treadmill Switchboard.
//!
//! These commands are useful for managing a switchboard instance, such as for
//! boostrapping admin users, or adding users/groups to the allow-list.
//!
//! They talk to the database directly, with no authenticated subject, so every
//! write rides the audit chokepoint ([`audit::transition`]) attributed to the
//! system actor, and asks for interactive confirmation first.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::audit::model::Subject as AuditSubject;
use crate::audit::{self, SYSTEM_ACTOR_ID, Transition, WriteToken, events};
use crate::auth::engine::ADMINS_GROUP_ID;

#[derive(clap::Args, Debug)]
pub struct ManageCommand {
    #[arg(short = 'c', long = "config", env = "TML_CFG_FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    subcommand: ManageSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ManageSubcommand {
    /// Grant or revoke global admin authority.
    #[command(subcommand)]
    Admin(AdminSubcommand),
    /// Inspect and edit the login allow-list consulted for new registrations.
    #[command(subcommand)]
    Allowlist(AllowlistSubcommand),
    /// Create groups.
    #[command(subcommand)]
    Group(GroupSubcommand),
}

#[derive(Debug, clap::Subcommand)]
pub enum AdminSubcommand {
    /// Add a user to the `admins` group.
    Grant { user: UserRef },
    /// Remove a user's direct membership in the `admins` group.
    Revoke { user: UserRef },
}

#[derive(Debug, clap::Subcommand)]
pub enum AllowlistSubcommand {
    /// List every allow-list entry.
    List,
    /// Allow an external user or org to register.
    Add {
        kind: AllowlistKind,
        /// The provider's stable id, or (for GitHub) a login handle, which is
        /// resolved to the id it currently names. Entries always key on the id:
        /// a handle can be renamed away and re-registered by someone else.
        external_id: String,
        #[arg(long, default_value = "github")]
        provider: String,
        /// Free-form note recorded alongside the entry (e.g. who asked for it).
        /// Defaults to the handle the entry was added by.
        #[arg(long)]
        comment: Option<String>,
    },
    /// Remove an allow-list entry. Already-registered users keep their account.
    Remove {
        kind: AllowlistKind,
        /// The provider's stable id, or (for GitHub) a login handle. A handle
        /// resolves to the id it names *today*, which is not necessarily the
        /// one on the entry -- check `allowlist list` if in doubt.
        external_id: String,
        #[arg(long, default_value = "github")]
        provider: String,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum GroupSubcommand {
    /// Create an empty group, optionally auto-synced from a GitHub org.
    Create {
        name: String,
        /// The org's stable numeric id, or its login handle (resolved to the
        /// id). Members of this org are added to the group by the auto-group
        /// reconciler as they log in.
        #[arg(long)]
        github_org: Option<String>,
        /// The org's login handle, recorded for display only. Defaults to the
        /// handle the id resolves to.
        #[arg(long, requires = "github_org")]
        github_org_name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AllowlistKind {
    User,
    Org,
}

impl AllowlistKind {
    /// The value stored in `login_allowlist.kind`.
    fn as_str(self) -> &'static str {
        match self {
            AllowlistKind::User => "user",
            AllowlistKind::Org => "org",
        }
    }
}

/// How a user is named on the command line: either their internal subject id,
/// or a provider login handle (which must resolve to exactly one account).
#[derive(Debug, Clone)]
pub enum UserRef {
    Id(Uuid),
    Login(String),
}

impl std::str::FromStr for UserRef {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match Uuid::parse_str(s) {
            Ok(id) => UserRef::Id(id),
            Err(_) => UserRef::Login(s.to_string()),
        })
    }
}

impl ManageCommand {
    pub async fn run(self) -> anyhow::Result<()> {
        let config = crate::config::load_configuration(self.config.as_deref())?;
        let pool = crate::serve::pg_pool_from_config(&config.database)
            .await
            .context("failed to connect to database")?;

        // Handle lookups go to the same API the login flow uses, so a GitHub
        // Enterprise deployment resolves against its own instance.
        let github_api = match &config.oauth.github {
            Some(gh) => gh.api_base_url.trim_end_matches('/').to_string(),
            None => crate::config::default_github_api_base_url(),
        };

        match self.subcommand {
            ManageSubcommand::Admin(cmd) => cmd.run(&pool).await,
            ManageSubcommand::Allowlist(cmd) => cmd.run(&pool, &github_api).await,
            ManageSubcommand::Group(cmd) => cmd.run(&pool, &github_api).await,
        }
    }
}

impl AdminSubcommand {
    async fn run(self, pool: &PgPool) -> anyhow::Result<()> {
        let (user, granted) = match self {
            AdminSubcommand::Grant { user } => (user, true),
            AdminSubcommand::Revoke { user } => (user, false),
        };

        let (user_id, name) = resolve_user(pool, &user).await?;

        // Only direct `manual` membership is ours to change: a user may also
        // hold admin authority transitively (through a nested or auto-synced
        // group), which revoking here does not take away.
        let member = sqlx::query_scalar!(
            r#"select exists (
                select 1 from tml_switchboard.group_members
                where group_id = $1 and member_id = $2 and source = 'manual'
            ) as "member!";"#,
            ADMINS_GROUP_ID,
            user_id,
        )
        .fetch_one(pool)
        .await?;

        if member == granted {
            println!(
                "{name} ({user_id}) is already {}an admin; nothing to do",
                if granted { "" } else { "not " }
            );
            return Ok(());
        }

        let verb = if granted { "GRANT" } else { "REVOKE" };
        if !confirm(&format!("{verb} admin authority for {name} ({user_id})?"))? {
            bail!("aborted");
        }

        let mut txn = pool.begin().await?;
        audit::transition(&mut txn, SetAdmin { user_id, granted }).await?;
        txn.commit().await?;

        println!("done");
        Ok(())
    }
}

impl AllowlistSubcommand {
    async fn run(self, pool: &PgPool, github_api: &str) -> anyhow::Result<()> {
        let (provider, kind, external_id, comment, added) = match self {
            AllowlistSubcommand::List => return list_allowlist(pool).await,
            AllowlistSubcommand::Add {
                kind,
                external_id,
                provider,
                comment,
            } => (provider, kind, external_id, comment, true),
            AllowlistSubcommand::Remove {
                kind,
                external_id,
                provider,
            } => (provider, kind, external_id, None, false),
        };

        let kind_str = kind.as_str();

        // The table keys on the provider's stable id; accept the handle too.
        let (external_id, handle) = match provider.as_str() {
            "github" => resolve_github(github_api, &external_id).await?,
            _ => (external_id, None),
        };
        let display = match &handle {
            Some(h) => format!("{external_id} ({h})"),
            None => external_id.clone(),
        };
        // With no note of its own, an entry records the handle it was added
        // by -- the id alone tells a later reader nothing.
        let comment = comment.or(handle);

        let present = sqlx::query_scalar!(
            r#"select exists (
                select 1 from tml_switchboard.login_allowlist
                where provider = $1 and kind = $2 and external_id = $3
            ) as "present!";"#,
            provider,
            kind_str,
            external_id,
        )
        .fetch_one(pool)
        .await?;

        if present == added {
            println!(
                "{provider} {kind_str} {display} is already {}allowed; nothing to do",
                if added { "" } else { "not " }
            );
            return Ok(());
        }

        let verb = if added { "ALLOW" } else { "DISALLOW" };
        if !confirm(&format!(
            "{verb} registration for {provider} {kind_str} {display}?"
        ))? {
            bail!("aborted");
        }

        let mut txn = pool.begin().await?;
        audit::transition(
            &mut txn,
            SetAllowlistEntry {
                provider,
                kind,
                external_id,
                comment,
                added,
            },
        )
        .await?;
        txn.commit().await?;

        println!("done");
        Ok(())
    }
}

async fn list_allowlist(pool: &PgPool) -> anyhow::Result<()> {
    let entries = sqlx::query!(
        "select provider, kind, external_id, comment, added_at \
         from tml_switchboard.login_allowlist \
         order by provider, kind, external_id;",
    )
    .fetch_all(pool)
    .await?;

    let n = entries.len();
    println!("{n} allow-list entr{}", if n == 1 { "y" } else { "ies" });
    for e in &entries {
        println!(
            "{:<10} {:<5} {:<24} {}  {}",
            e.provider,
            e.kind,
            e.external_id,
            e.added_at.format("%Y-%m-%d"),
            e.comment.as_deref().unwrap_or(""),
        );
    }
    Ok(())
}

impl GroupSubcommand {
    async fn run(self, pool: &PgPool, github_api: &str) -> anyhow::Result<()> {
        let GroupSubcommand::Create {
            name,
            github_org,
            github_org_name,
        } = self;

        // The binding keys on the org's stable id; accept its handle too.
        let (github_org, github_org_name) = match github_org {
            Some(org) => {
                let (id, handle) = resolve_github(github_api, &org).await?;
                (Some(id), github_org_name.or(handle))
            }
            None => (None, None),
        };

        let bound_to = match (&github_org, &github_org_name) {
            (Some(id), Some(handle)) => format!(", auto-synced from github org {id} ({handle})"),
            (Some(id), None) => format!(", auto-synced from github org {id}"),
            (None, _) => String::new(),
        };
        if !confirm(&format!("CREATE group {name}{bound_to}?"))? {
            bail!("aborted");
        }

        let mut txn = pool.begin().await?;
        let group_id = audit::transition(
            &mut txn,
            CreateGroup {
                name,
                github_org,
                github_org_name,
            },
        )
        .await?;
        txn.commit().await?;

        println!("created group {group_id}");
        Ok(())
    }
}

/// Look up the subject id and display name of the user named on the command
/// line. A login handle must match exactly one account across all providers.
async fn resolve_user(pool: &PgPool, user: &UserRef) -> anyhow::Result<(Uuid, String)> {
    match user {
        UserRef::Id(id) => sqlx::query!(
            "select subject_id, name from tml_switchboard.users where subject_id = $1;",
            id,
        )
        .fetch_optional(pool)
        .await?
        .map(|r| (r.subject_id, r.name))
        .with_context(|| format!("no user with subject id {id}")),

        UserRef::Login(login) => {
            let mut rows = sqlx::query!(
                "select distinct u.subject_id, u.name \
                 from tml_switchboard.users u \
                 join tml_switchboard.user_identities i on i.user_id = u.subject_id \
                 where lower(i.provider_login) = lower($1);",
                login,
            )
            .fetch_all(pool)
            .await?;

            match rows.len() {
                0 => bail!("no user with provider login {login}"),
                1 => {
                    let r = rows.remove(0);
                    Ok((r.subject_id, r.name))
                }
                n => bail!("{login} matches {n} users; name one by subject id instead"),
            }
        }
    }
}

/// Resolve a GitHub user or org handle to the stable numeric id everything is
/// keyed on, returning `(id, handle)`. An all-digits argument is already an id
/// and is taken as-is (with no handle to report), so both forms work on the
/// command line. Handles are renameable and are only ever carried alongside
/// the id, for display.
async fn resolve_github(
    api_base_url: &str,
    name: &str,
) -> anyhow::Result<(String, Option<String>)> {
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Ok((name.to_string(), None));
    }

    #[derive(serde::Deserialize)]
    struct GhAccount {
        id: i64,
        login: String,
    }

    // `/users/{handle}` resolves organizations too (as `type: Organization`).
    let url = format!("{api_base_url}/users/{name}");
    let account: GhAccount = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "treadmill-switchboard")
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("no such GitHub user or org: {name}"))?
        .json()
        .await
        .with_context(|| format!("GET {url}: decoding response"))?;

    Ok((account.id.to_string(), Some(account.login)))
}

/// Ask on the terminal before a write proceeds. Anything but a literal `yes`
/// (including a closed stdin, e.g. when run non-interactively) declines.
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt} [yes/no] ");
    io::stdout().flush()?;

    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        println!();
        return Ok(false);
    }
    Ok(line.trim() == "yes")
}

/// Add or remove a user's direct `manual` membership in the `admins` group.
/// Emits [`events::AdminAuthorityChanged`].
struct SetAdmin {
    user_id: Uuid,
    granted: bool,
}

impl Transition for SetAdmin {
    type Output = ();
    type Event = events::AdminAuthorityChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        if self.granted {
            sqlx::query!(
                "insert into tml_switchboard.group_members (group_id, member_id, source) \
                 values ($1, $2, 'manual') on conflict do nothing;",
                ADMINS_GROUP_ID,
                self.user_id,
            )
            .execute(&mut *conn)
            .await?;
        } else {
            sqlx::query!(
                "delete from tml_switchboard.group_members \
                 where group_id = $1 and member_id = $2 and source = 'manual';",
                ADMINS_GROUP_ID,
                self.user_id,
            )
            .execute(&mut *conn)
            .await?;
        }

        let event = events::AdminAuthorityChanged {
            actor: AuditSubject(SYSTEM_ACTOR_ID),
            user: AuditSubject(self.user_id),
            granted: self.granted,
        };
        Ok(((), event))
    }
}

/// Add or remove one `login_allowlist` entry. Emits
/// [`events::LoginAllowlistChanged`].
struct SetAllowlistEntry {
    provider: String,
    kind: AllowlistKind,
    external_id: String,
    comment: Option<String>,
    added: bool,
}

impl Transition for SetAllowlistEntry {
    type Output = bool;
    type Event = events::LoginAllowlistChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        let kind = self.kind.as_str();
        let changed = if self.added {
            sqlx::query!(
                "insert into tml_switchboard.login_allowlist \
                 (provider, kind, external_id, comment) values ($1, $2, $3, $4) \
                 on conflict (provider, kind, external_id) do nothing;",
                self.provider,
                kind,
                self.external_id,
                self.comment,
            )
            .execute(&mut *conn)
            .await?
        } else {
            sqlx::query!(
                "delete from tml_switchboard.login_allowlist \
                 where provider = $1 and kind = $2 and external_id = $3;",
                self.provider,
                kind,
                self.external_id,
            )
            .execute(&mut *conn)
            .await?
        }
        .rows_affected()
            > 0;

        let event = events::LoginAllowlistChanged {
            actor: AuditSubject(SYSTEM_ACTOR_ID),
            provider: self.provider,
            kind: kind.to_string(),
            external_id: self.external_id,
            added: self.added,
        };
        Ok((changed, event))
    }
}

/// Create an empty group, optionally bound to a GitHub org whose members the
/// auto-group reconciler folds in. Emits [`events::GroupCreated`].
struct CreateGroup {
    name: String,
    github_org: Option<String>,
    github_org_name: Option<String>,
}

impl Transition for CreateGroup {
    type Output = Uuid;
    type Event = events::GroupCreated;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        // Time-ordered (v7) for primary-key insert locality.
        let group_id = Uuid::now_v7();
        sqlx::query!(
            "insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'group');",
            group_id,
        )
        .execute(&mut *conn)
        .await?;

        sqlx::query!(
            "insert into tml_switchboard.groups (subject_id, name) values ($1, $2);",
            group_id,
            self.name,
        )
        .execute(&mut *conn)
        .await?;

        if let Some(external_id) = &self.github_org {
            sqlx::query!(
                "insert into tml_switchboard.group_auto_sources \
                 (group_id, provider, external_id, external_name, membership_via) \
                 values ($1, 'github', $2, $3, 'github_org');",
                group_id,
                external_id,
                self.github_org_name,
            )
            .execute(&mut *conn)
            .await?;
        }

        let event = events::GroupCreated {
            actor: AuditSubject(SYSTEM_ACTOR_ID),
            group: AuditSubject(group_id),
            name: self.name,
            auto_source_provider: self.github_org.as_ref().map(|_| "github".to_string()),
            auto_source_external_id: self.github_org,
        };
        Ok((group_id, event))
    }
}
