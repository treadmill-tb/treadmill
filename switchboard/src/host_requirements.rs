//! The host-matching diagnostic, shared by enqueue and the dry-run route.
//!
//! A job whose requirements match nothing is accepted and then sits queued
//! forever, which looks identical to a fleet that is merely busy. This reports
//! the difference at submission.
//!
//! Unlike the scheduler, which stops at the first host that works, this
//! evaluates **both** filters over the whole authorized set: short-circuiting
//! is a hot-path optimization, and letting it leak here would collapse "your
//! predicate matched nothing" and "no image-set member admits the hosts it
//! matched" into the same empty answer.

use sqlx::PgPool;
use treadmill_rs::api::switchboard::hosts::{HostPredicateError, HostRequirementsReport};
use treadmill_rs::host_spec::HostSpecV1;
use uuid::Uuid;

use crate::matcher::{GroupMember, select_member};
use crate::predicate::{CelEngine, Engine};
use crate::sql;

/// How many evaluation errors are listed individually. A predicate missing a
/// guard errors on every host of a kind, and the first few say what the rest
/// would; `HostRequirementsReport::errored` still carries the true total.
const MAX_REPORTED_ERRORS: usize = 10;

/// Evaluate `predicate`, and optionally an image set's members, against every
/// host `owner` may start on.
///
/// `image_set` is `(set_id, generation)` with the generation already resolved,
/// so this reports on the same membership an enqueued job would freeze.
pub async fn evaluate(
    pool: &PgPool,
    owner: Uuid,
    predicate: &str,
    image_set: Option<(Uuid, u32)>,
) -> Result<HostRequirementsReport, sqlx::Error> {
    let hosts = sql::host::authorized_for_subject(owner, pool).await?;
    let specs = sql::host_spec::current_for_all_hosts(pool)
        .await?
        .into_iter()
        .filter_map(|row| match row {
            Ok(stored) => Some((stored.host_id, stored.normalize())),
            // As in a scheduling pass: one unreadable document must not fail
            // the whole report. Such a host counts as matching nothing, which
            // is also how the scheduler treats it.
            Err(e) => {
                tracing::error!("skipping host spec in a requirements report: {e}");
                None
            }
        })
        .collect::<std::collections::HashMap<_, _>>();

    let mut report = HostRequirementsReport {
        authorized: hosts.len() as u32,
        predicate_matched: 0,
        image_matched: None,
        schedulable: Vec::new(),
        errored: 0,
        errors: Vec::new(),
        compile_error: None,
    };

    let compiled = match CelEngine.compile(predicate) {
        Ok(compiled) => compiled,
        // Nothing to evaluate; the counts stay zero and the caller is told why
        // rather than being handed a plausible-looking empty match.
        Err(e) => {
            report.compile_error = Some(e.to_string());
            return Ok(report);
        }
    };

    let members = match image_set {
        Some((set_id, generation)) => {
            let rows = sql::image::members_for_generation(pool, set_id, generation).await?;
            report.image_matched = Some(0);
            Some(
                rows.into_iter()
                    .map(|m| GroupMember {
                        handle: (),
                        platform_profile: m.platform_profile,
                        predicate: m.predicate,
                    })
                    .collect::<Vec<_>>(),
            )
        }
        None => None,
    };

    for host in hosts {
        let spec: Option<&HostSpecV1> = specs.get(&host.host_id);

        // An undescribed host is dispatchable by nothing, so it neither
        // matches nor errors; it is simply one of `authorized`.
        let admitted = match spec {
            Some(spec) => match compiled.eval(spec) {
                Ok(admitted) => admitted,
                Err(e) => {
                    report.errored += 1;
                    if report.errors.len() < MAX_REPORTED_ERRORS {
                        report.errors.push(HostPredicateError {
                            host_id: host.host_id,
                            name: host.name.clone(),
                            message: e.to_string(),
                        });
                    }
                    false
                }
            },
            None => false,
        };
        if admitted {
            report.predicate_matched += 1;
        }

        let Some(members) = members.as_deref() else {
            if admitted {
                report.schedulable.push(host.host_id);
            }
            continue;
        };
        let has_member = select_member(members, spec).is_some();
        if has_member {
            report.image_matched = Some(report.image_matched.unwrap_or(0) + 1);
        }
        if admitted && has_member {
            report.schedulable.push(host.host_id);
        }
    }

    Ok(report)
}
