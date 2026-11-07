//! Naming a job, and finding the one a short name means.

use uuid::Uuid;

/// How many of a UUID's trailing hex digits [`short`] keeps. Long enough to
/// be unambiguous in practice, short enough to read.
const SHORT_LEN: usize = 8;

/// A job's UUID abbreviated for display, as the `^<hex>` suffix that names it:
/// what [`short`] returns parses back as the [`JobRef`] for the same job.
pub fn short(job_id: Uuid) -> String {
    let text = job_id.simple().to_string();
    format!("^{}", &text[text.len() - SHORT_LEN..])
}

/// A job named either in full, or by `^<hex>` — a suffix of its UUID, which
/// the switchboard resolves and which must match exactly one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRef {
    Id(Uuid),
    Short(String),
}

impl JobRef {
    /// Whether this reference names `id`. A suffix matches the end of the
    /// UUID's unhyphenated form.
    pub fn matches(&self, id: Uuid) -> bool {
        match self {
            Self::Id(own) => *own == id,
            Self::Short(suffix) => id.simple().to_string().ends_with(suffix.as_str()),
        }
    }
}

impl std::str::FromStr for JobRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let Some(hex) = s.strip_prefix('^') else {
            return Uuid::parse_str(s)
                .map(Self::Id)
                .map_err(|e| format!("not a UUID or ^<hex> suffix: {e}"));
        };
        if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("expected ^ followed by 1 to 32 hex digits".to_string());
        }
        // ^ with all 32 digits is a full UUID.
        Ok(Uuid::parse_str(hex).map_or_else(|_| Self::Short(hex.to_ascii_lowercase()), Self::Id))
    }
}

#[cfg(feature = "profile")]
mod resolve {
    use anyhow::{Context as _, Result, bail};
    use treadmill_rs::api::switchboard::jobs::{JobListQuery, JobListState};
    use uuid::Uuid;

    use super::JobRef;
    use crate::session::Session;

    /// The UUID `reference` names, or the session's active job if it names
    /// none.
    ///
    /// A `^<hex>` suffix is looked up on the switchboard, across every list a
    /// job can be in, and must match exactly one. Which job was picked is
    /// reported as detail, since the caller did not name it outright.
    pub async fn resolve(session: &Session, reference: Option<&JobRef>) -> Result<Uuid> {
        let suffix = match reference {
            Some(JobRef::Id(job)) => return Ok(*job),
            Some(JobRef::Short(suffix)) => suffix,
            None => {
                return session
                    .state
                    .active_job
                    .context("no job given and no active job set");
            }
        };

        let client = session.authenticated_client()?;
        let queries = [
            JobListState::Active,
            JobListState::Reclaimable,
            JobListState::Finished,
        ]
        .map(|state| JobListQuery {
            include: "all".to_string(),
            state,
            q: Some(format!("^{suffix}")),
            limit: Some(2),
            cursor: None,
        });
        let pages =
            futures_util::future::try_join_all(queries.iter().map(|query| client.list_jobs(query)))
                .await
                .with_context(|| format!("looking up job ^{suffix}"))?;

        let jobs: Vec<Uuid> = pages
            .into_iter()
            .flat_map(|page| page.jobs)
            .map(|job| job.job_id)
            .collect();
        match jobs[..] {
            [job] => {
                session.diagnostics().detail(&format!("^{suffix} → {job}"));
                Ok(job)
            }
            [] => bail!("no job matches ^{suffix}"),
            _ => bail!(
                "^{suffix} matches several jobs, including {}; give more digits",
                jobs.iter()
                    .map(Uuid::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[cfg(feature = "profile")]
pub use resolve::resolve;

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6";

    #[test]
    fn a_job_ref_is_a_uuid_or_a_suffix() {
        let id = Uuid::parse_str(JOB).unwrap();
        assert_eq!(JOB.parse(), Ok(JobRef::Id(id)));
        assert_eq!(format!("^{}", id.simple()).parse(), Ok(JobRef::Id(id)));
        assert_eq!("^D3E4f5a6".parse(), Ok(JobRef::Short("d3e4f5a6".into())));
    }

    #[test]
    fn a_malformed_job_ref_is_refused() {
        for bad in [
            "",
            "^",
            "^xyz",
            "d3e4f5a6",
            "^-d3e4",
            &format!("^0{}", JOB.replace('-', "")),
        ] {
            assert!(bad.parse::<JobRef>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_suffix_matches_the_end_of_a_uuid() {
        let id = Uuid::parse_str(JOB).unwrap();
        assert!("^d3e4f5a6".parse::<JobRef>().unwrap().matches(id));
        assert!(!"^0192f3a4".parse::<JobRef>().unwrap().matches(id));
        assert!(JOB.parse::<JobRef>().unwrap().matches(id));
        assert!(JobRef::Short("f5a6".into()).matches(id));
        assert!(!JobRef::Short("f5a7".into()).matches(id));
    }

    /// What `short` prints has to be something the user can pass straight
    /// back in, so it round-trips through `JobRef`.
    #[test]
    fn a_short_id_round_trips_as_a_job_ref() {
        let id = Uuid::parse_str(JOB).unwrap();
        assert_eq!(short(id), "^d3e4f5a6");
        assert_eq!(short(id).parse(), Ok(JobRef::Short("d3e4f5a6".into())));
        assert!(short(id).parse::<JobRef>().unwrap().matches(id));
    }
}
