//! Asking the in-job `tml daemon` for the job's own credentials.
//!
//! A process running inside a Treadmill job does not log in. The supervisor
//! hands the job a switchboard token of its own, and `tml daemon` publishes it
//! on D-Bus, so anything in the image can act as the job without a user
//! session. This module is the client side of that: the daemon itself lives in
//! `treadmill-cli`.
//!
//! The interface is declared twice — as the proxy below, and as the `#[zbus::interface]`
//! the daemon implements. That is what makes it a contract between separately
//! built programs rather than an internal detail, but the two have to be kept
//! in step by hand.

use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::job::JobRef;

/// Which bus the daemon is on. An image runs it on the system bus; a test or
/// a development run may use the session bus instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbusBus {
    Session,
    System,
}

/// The interface name this proxy asks for. `treadmill-cli` asserts that the
/// daemon it builds serves exactly this, with exactly [`DAEMON_METHODS`].
pub const DAEMON_INTERFACE: &str = "dev.treadmill.Daemon1";

/// The methods [`DaemonProxy`] calls, in the D-Bus spelling the `#[proxy]`
/// macro derives from the trait below.
pub const DAEMON_METHODS: &[&str] = &["Credentials", "ReloadServices"];

/// The D-Bus interface `tml daemon` serves. Kept in step by hand with the
/// `#[zbus::interface]` implementation in `treadmill-cli`.
#[zbus::proxy(
    interface = "dev.treadmill.Daemon1",
    default_path = "/dev/treadmill/Daemon",
    default_service = "dev.treadmill.Daemon"
)]
pub trait Daemon {
    /// The job's switchboard base URL, token, and id.
    fn credentials(&self) -> zbus::Result<(String, String, String)>;

    /// Re-scan the declared services and announce them again.
    fn reload_services(&self) -> zbus::Result<()>;
}

/// A job's own switchboard credentials, as the daemon reports them.
#[derive(Clone)]
pub struct Credentials {
    pub base_url: String,
    pub token: String,
    pub job_id: Uuid,
}

impl Credentials {
    /// A client authenticated as the job.
    pub fn client(&self) -> SwitchboardClient {
        SwitchboardClient::new(self.base_url.clone(), Some(self.token.clone()))
    }

    /// The job these credentials are for, checking that `job` — if it names
    /// one at all — names that job.
    ///
    /// A job's token only authorizes the job itself, so naming another one is
    /// a mistake worth catching here rather than as a 403 from the
    /// switchboard.
    pub fn check_job(&self, job: Option<&JobRef>) -> Result<Uuid> {
        if let Some(job) = job
            && !job.matches(self.job_id)
        {
            bail!(
                "inside a job, only that job ({}) can be acted on",
                self.job_id
            );
        }
        Ok(self.job_id)
    }
}

/// Open a connection to `bus`.
pub async fn connect(bus: DbusBus) -> zbus::Result<zbus::Connection> {
    match bus {
        DbusBus::System => zbus::Connection::system().await,
        DbusBus::Session => zbus::Connection::session().await,
    }
}

/// The job's credentials, or `None` if this is not running inside a job with
/// a `tml daemon` on `bus`.
///
/// A bus that cannot be reached at all is `None` rather than an error: not
/// being inside a job is the ordinary case for a client on a user's machine.
pub async fn credentials(bus: DbusBus) -> Result<Option<Credentials>> {
    let Ok(conn) = connect(bus).await else {
        return Ok(None);
    };
    let owned = zbus::fdo::DBusProxy::new(&conn)
        .await?
        .name_has_owner("dev.treadmill.Daemon".try_into()?)
        .await?;
    if !owned {
        return Ok(None);
    }

    let (base_url, token, job_id) = DaemonProxy::new(&conn)
        .await?
        .credentials()
        .await
        .context("asking the tml daemon for its credentials")?;
    Ok(Some(Credentials {
        base_url,
        token,
        job_id: job_id
            .parse()
            .context("the tml daemon sent a malformed job id")?,
    }))
}

/// Ask the daemon to re-scan and re-announce the job's services.
pub async fn reload_services(bus: DbusBus) -> Result<()> {
    DaemonProxy::new(&connect(bus).await?)
        .await?
        .reload_services()
        .await
        .context("requesting a service reload")
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb";

    fn credentials() -> Credentials {
        Credentials {
            base_url: "https://sb.example".into(),
            token: "token".into(),
            job_id: Uuid::parse_str(JOB).unwrap(),
        }
    }

    #[test]
    fn naming_no_job_means_the_running_one() {
        assert_eq!(credentials().check_job(None).unwrap(), credentials().job_id);
    }

    #[test]
    fn naming_the_running_job_is_allowed_in_either_form() {
        let credentials = credentials();
        assert!(credentials.check_job(Some(&JOB.parse().unwrap())).is_ok());
        assert!(
            credentials
                .check_job(Some(&"^b9a761bde3fb".parse().unwrap()))
                .is_ok()
        );
    }

    #[test]
    fn naming_another_job_is_refused() {
        let other: JobRef = "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6".parse().unwrap();
        assert!(credentials().check_job(Some(&other)).is_err());
    }
}
