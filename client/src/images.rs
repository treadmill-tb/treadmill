//! Publishing an image to a switchboard's catalog.
//!
//! Pushing an image to a registry does not make it runnable: the switchboard
//! has to be told the digest exists and where to pull it from, whoever should
//! run it has to be granted `use`, and the image set a job selects by name has
//! to be rolled forward to include it. That is three interactions with four
//! routes, and each has a rule that is easy to get wrong on its own:
//!
//! - registering races the registry, and has to be retried ([`register`]);
//! - the source a caller may grant on is not always the one it just submitted
//!   ([`manageable_source`]);
//! - a new generation replaces a set's membership *wholesale*, so members that
//!   were not rebuilt have to be carried forward explicitly
//!   ([`plan_generation`]).
//!
//! What is deliberately not here: where the digests came from, and what names
//! the sets go by. Those belong to whatever drives the publish — for `tml`,
//! a `publish.json` and the records `tml image push` leaves behind.

use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::time::Instant;
use treadmill_rs::api::switchboard::client::{AddedImageSource, ClientError, SwitchboardClient};
use treadmill_rs::api::switchboard::images::{
    AddImageSourceRequest, GenerationMemberInfo, GenerationMemberSpec, ImageInfo,
    ImageSetGenerationInfo, ImageSetInfo, ImageSourceGrantRequest, ImageSourcePermission,
};
use treadmill_rs::image::Digest;
use uuid::Uuid;

use crate::diagnostics::Diagnostics;

/// The well-known subject that every caller is a member of. Granting it `use`
/// on an image source is what makes that image public.
pub const EVERYONE_SUBJECT_ID: Uuid = Uuid::from_u128(4);

/// How long [`register`] keeps trying by default.
///
/// Generous because the thing being waited on is a third-party registry making
/// a just-accepted push readable, which has no upper bound worth guessing at.
pub const DEFAULT_REGISTER_TIMEOUT: Duration = Duration::from_secs(900);

/// The delay before the first retry; doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(15);

/// The ceiling on the retry delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Whether a failed registration is worth trying again.
///
/// Registering is the one route that fails transiently in normal operation:
/// the switchboard pulls the manifest from the submitted source to verify it
/// before recording anything, and a registry that has just accepted a push
/// routinely cannot serve it yet. That surfaces as `502`. The other statuses
/// here are the usual transient ones, and a transport failure never says
/// anything about the request itself.
///
/// Everything else — `401`, `403`, `404`, `422` — is a fact about the request
/// that will not change by asking again.
pub fn is_retryable(error: &ClientError) -> bool {
    match error {
        ClientError::Transport(_) => true,
        ClientError::Unauthorized => false,
        ClientError::Status { status, .. } => matches!(status, 429 | 502 | 503 | 504),
    }
}

/// The delay to wait after having waited `previous`.
fn next_backoff(previous: Duration) -> Duration {
    previous.saturating_mul(2).min(MAX_BACKOFF)
}

/// Record that `digest` can be pulled from `registry`/`repository`, retrying
/// for up to `timeout` while the answer is one [`is_retryable`] accepts.
///
/// Idempotent: a source already recorded for the digest is a no-op, and the
/// returned [`AddedImageSource::registered`] says whether the digest was new.
pub async fn register(
    client: &SwitchboardClient,
    digest: &Digest,
    registry: &str,
    repository: &str,
    timeout: Duration,
    diagnostics: &dyn Diagnostics,
) -> Result<AddedImageSource> {
    let request = AddImageSourceRequest {
        registry: registry.to_string(),
        repository: repository.to_string(),
    };
    let deadline = Instant::now() + timeout;
    let mut delay = FIRST_BACKOFF;

    loop {
        let error = match client.add_image_source(digest, &request).await {
            Ok(added) => return Ok(added),
            Err(error) => error,
        };
        if !is_retryable(&error) {
            return Err(error).with_context(|| format!("registering {repository}@{digest}"));
        }

        // Waiting past the deadline is the same as not waiting: report the
        // failure that was actually seen, not a bare timeout.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error).with_context(|| {
                format!(
                    "gave up registering {repository}@{digest} after {}s",
                    timeout.as_secs()
                )
            });
        }

        let wait = delay.min(remaining);
        diagnostics.detail(&format!(
            "{repository}@{digest} is not registrable yet ({error}); retrying in {}s",
            wait.as_secs()
        ));
        tokio::time::sleep(wait).await;
        delay = next_backoff(delay);
    }
}

/// The source among `image`'s that this caller may manage and that matches the
/// `registry`/`repository` it submitted.
///
/// `None` means someone else got there first: the digest was already sourced
/// from the same repository by another subject, so the add was a no-op, their
/// grants are the ones in force, and this caller has nothing to grant on.
pub fn manageable_source(image: &ImageInfo, registry: &str, repository: &str) -> Option<Uuid> {
    image
        .sources
        .iter()
        .find(|source| {
            source.registry == registry
                && source.repository == repository
                && source.permissions.contains(&ImageSourcePermission::Manage)
        })
        .map(|source| source.id)
}

/// Grant the well-known `everyone` subject `use` on one source, which is what
/// makes the image behind it public.
pub async fn grant_everyone(
    client: &SwitchboardClient,
    digest: &Digest,
    source_id: Uuid,
) -> Result<()> {
    client
        .grant_image_source(
            digest,
            source_id,
            &ImageSourceGrantRequest {
                subject_id: EVERYONE_SUBJECT_ID,
                permission: ImageSourcePermission::Use,
            },
        )
        .await
        .with_context(|| format!("granting `use` on {digest} to everyone"))
}

/// One member a caller wants the next generation to have.
#[derive(Debug, Clone)]
pub struct DesiredMember {
    /// The machine configuration this member serves, matched by equality
    /// against a host's advertised profiles.
    pub platform_profile: String,
    /// A CEL refinement narrowing this member within its profile, if any.
    pub predicate: Option<String>,
    /// The digest to use. `None` asks for whatever the current generation has
    /// for the same profile and predicate — i.e. "this one was not rebuilt".
    pub manifest_digest: Option<Digest>,
}

impl DesiredMember {
    /// How to name this member in a message, when there is no digest to name
    /// it by.
    fn label(&self) -> String {
        match &self.predicate {
            Some(predicate) => format!("{} ({predicate})", self.platform_profile),
            None => self.platform_profile.clone(),
        }
    }
}

/// The members of the next generation, or `None` if they are exactly what the
/// set already has.
///
/// A generation replaces a set's membership wholesale, so every member has to
/// be named every time: one rebuilt in this run takes its new digest, and one
/// that was not keeps the digest the current generation gave the same
/// `(platform_profile, predicate)` pair. A member that is neither is an error
/// rather than an omission — publishing it would quietly drop that platform
/// out of the set, and the next job to want it would find nothing.
///
/// The `None` result is what keeps a set from gaining a generation every time
/// anything at all is published: the switchboard would accept a generation
/// identical to its predecessor quite happily.
pub fn plan_generation(
    current: &[GenerationMemberInfo],
    desired: &[DesiredMember],
) -> Result<Option<Vec<GenerationMemberSpec>>> {
    let mut members = Vec::with_capacity(desired.len());
    for member in desired {
        let manifest_digest = match member.manifest_digest {
            Some(digest) => digest,
            None => current
                .iter()
                .find(|existing| {
                    existing.platform_profile == member.platform_profile
                        && existing.predicate == member.predicate
                })
                .map(|existing| existing.manifest_digest)
                .with_context(|| {
                    format!(
                        "{} was not built, and the set's current generation has no member \
                         for it to carry forward",
                        member.label()
                    )
                })?,
        };
        members.push(GenerationMemberSpec {
            manifest_digest,
            platform_profile: member.platform_profile.clone(),
            predicate: member.predicate.clone(),
        });
    }

    if is_unchanged(current, &members) {
        return Ok(None);
    }
    Ok(Some(members))
}

/// Whether `next` says the same thing as `current`. Order is part of it:
/// a member's position is its `index`, the tie-break among equally-specific
/// members, so reordering is a real change.
fn is_unchanged(current: &[GenerationMemberInfo], next: &[GenerationMemberSpec]) -> bool {
    current.len() == next.len()
        && current.iter().zip(next).all(|(current, next)| {
            current.manifest_digest == next.manifest_digest
                && current.platform_profile == next.platform_profile
                && current.predicate == next.predicate
        })
}

/// The set called `canonical_name`, among those the caller can see.
pub fn find_set<'a>(sets: &'a [ImageSetInfo], canonical_name: &str) -> Option<&'a ImageSetInfo> {
    sets.iter()
        .find(|set| set.canonical_name.as_deref() == Some(canonical_name))
}

/// The members of `generation` that some holder of the set's `use` grant
/// cannot pull.
///
/// Non-empty means the set is grantable but not usable: whoever holds `use` on
/// the set still has no source they can reach for those members, so a job
/// naming the set would fail to resolve an image. The usual cause is an image
/// whose own source was never granted out (see [`grant_everyone`]).
pub fn unusable_members(generation: &ImageSetGenerationInfo) -> Vec<&GenerationMemberInfo> {
    generation
        .members
        .iter()
        .filter(|member| !member.usable_by_grantees)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> Digest {
        Digest::from_sha256([byte; 32])
    }

    fn current(members: &[(Digest, &str, Option<&str>)]) -> Vec<GenerationMemberInfo> {
        members
            .iter()
            .enumerate()
            .map(
                |(index, (digest, profile, predicate))| GenerationMemberInfo {
                    manifest_digest: *digest,
                    platform_profile: profile.to_string(),
                    predicate: predicate.map(str::to_string),
                    index: index as u32,
                    usable: true,
                    usable_by_grantees: true,
                },
            )
            .collect()
    }

    fn desired(profile: &str, predicate: Option<&str>, digest: Option<Digest>) -> DesiredMember {
        DesiredMember {
            platform_profile: profile.to_string(),
            predicate: predicate.map(str::to_string),
            manifest_digest: digest,
        }
    }

    #[test]
    fn a_rebuilt_member_takes_its_new_digest() {
        let current = current(&[(digest(1), "q35-virtio-uefi", None)]);
        let planned = plan_generation(
            &current,
            &[desired("q35-virtio-uefi", None, Some(digest(2)))],
        )
        .unwrap()
        .expect("the digest changed, so the membership did");
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].manifest_digest, digest(2));
    }

    #[test]
    fn a_member_that_was_not_built_carries_its_digest_forward() {
        let current = current(&[
            (digest(1), "q35-virtio-uefi", None),
            (digest(2), "arm64-nbd-netboot", Some("host.vendor == 'rpi'")),
        ]);
        let planned = plan_generation(
            &current,
            &[
                desired("q35-virtio-uefi", None, Some(digest(9))),
                desired("arm64-nbd-netboot", Some("host.vendor == 'rpi'"), None),
            ],
        )
        .unwrap()
        .expect("one member changed");
        assert_eq!(planned[0].manifest_digest, digest(9));
        assert_eq!(planned[1].manifest_digest, digest(2));
    }

    #[test]
    fn the_predicate_is_part_of_what_a_member_is_carried_forward_by() {
        // Same profile, different predicate: there is nothing to carry forward
        // even though the profile matches.
        let current = current(&[(digest(1), "arm64-nbd-netboot", Some("host.vendor == 'a'"))]);
        let error = plan_generation(
            &current,
            &[desired(
                "arm64-nbd-netboot",
                Some("host.vendor == 'b'"),
                None,
            )],
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("was not built"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn an_unchanged_membership_plans_no_generation() {
        let current = current(&[
            (digest(1), "q35-virtio-uefi", None),
            (digest(2), "arm64-nbd-netboot", Some("host.vendor == 'rpi'")),
        ]);
        let planned = plan_generation(
            &current,
            &[
                desired("q35-virtio-uefi", None, Some(digest(1))),
                desired("arm64-nbd-netboot", Some("host.vendor == 'rpi'"), None),
            ],
        )
        .unwrap();
        assert!(planned.is_none());
    }

    #[test]
    fn reordering_the_members_is_a_change() {
        // Position is index, which decides which of two admissible members a
        // host gets, so the same set in a different order is a new generation.
        let current = current(&[
            (digest(1), "q35-virtio-uefi", None),
            (digest(2), "arm64-nbd-netboot", None),
        ]);
        let planned = plan_generation(
            &current,
            &[
                desired("arm64-nbd-netboot", None, Some(digest(2))),
                desired("q35-virtio-uefi", None, Some(digest(1))),
            ],
        )
        .unwrap();
        assert!(planned.is_some());
    }

    #[test]
    fn dropping_a_member_is_a_change() {
        let current = current(&[
            (digest(1), "q35-virtio-uefi", None),
            (digest(2), "arm64-nbd-netboot", None),
        ]);
        let planned = plan_generation(
            &current,
            &[desired("q35-virtio-uefi", None, Some(digest(1)))],
        )
        .unwrap();
        assert_eq!(planned.expect("the set shrank").len(), 1);
    }

    #[test]
    fn a_first_generation_needs_every_member_built() {
        let error = plan_generation(&[], &[desired("q35-virtio-uefi", None, None)]).unwrap_err();
        assert!(
            format!("{error:#}").contains("q35-virtio-uefi"),
            "the error should name the member: {error:#}"
        );
    }

    #[test]
    fn only_the_transient_statuses_are_retried() {
        let retryable = |status| {
            is_retryable(&ClientError::Status {
                status,
                body: String::new(),
            })
        };
        // 502 is the one that matters: the switchboard could not pull the
        // manifest from the registry yet.
        assert!(retryable(502));
        assert!(retryable(429));
        assert!(retryable(503));
        assert!(retryable(504));

        assert!(!retryable(400));
        assert!(!retryable(403));
        assert!(!retryable(404));
        assert!(!retryable(422));
        assert!(!is_retryable(&ClientError::Unauthorized));
    }

    #[test]
    fn the_backoff_doubles_up_to_a_minute() {
        assert_eq!(next_backoff(FIRST_BACKOFF), Duration::from_secs(30));
        assert_eq!(next_backoff(Duration::from_secs(30)), MAX_BACKOFF);
        assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
    }
}
