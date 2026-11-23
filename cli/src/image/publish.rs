//! Telling the switchboard about images that have been pushed.
//!
//! Two commands, because they answer two different questions. `register` is
//! about one image: it exists, here is where to pull it from, and here is who
//! may. `publish` is about a *set*: given everything built in this run, what
//! should the set contain now?
//!
//! The policy both run on lives in [`treadmill_client::images`]. What is here
//! is reading the configuration, matching records to members, and saying what
//! happened.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use treadmill_client::images;
use treadmill_rs::api::switchboard::images::GenerationMemberSpec;

use crate::cli::{ImagePublishArgs, ImageRegisterArgs};
use crate::ctx::Ctx;
use crate::event::Event;

use super::{Member, ok, parse_digest, read_records, timeout};

/// A `publish.json`.
#[derive(Debug, Deserialize)]
struct Config {
    /// The switchboard these images belong to. Checked against the profile's,
    /// rather than used: publishing to the wrong one is not a mistake worth
    /// making quietly.
    #[serde(default)]
    switchboard: Option<String>,
    /// The registry authority recorded for every source.
    #[serde(default)]
    registry: Option<String>,
    /// The sets to roll forward, by canonical name.
    sets: BTreeMap<String, Vec<ConfigMember>>,
}

#[derive(Debug, Deserialize)]
struct ConfigMember {
    /// The coordinate whose push record supplies this member's digest.
    image: String,
    platform_profile: String,
    #[serde(default)]
    predicate: Option<String>,
}

/// `tml image register`: make one pushed image known and usable.
pub async fn register(ctx: &Ctx, args: &ImageRegisterArgs) -> Result<()> {
    let client = ctx.authenticated_client()?;
    let digest = parse_digest(&args.digest)?;

    ctx.note(&format!("Registering {}@{digest}", args.repository));
    let added = images::register(
        &client,
        &digest,
        &args.registry,
        &args.repository,
        timeout(args.timeout),
        ctx.diagnostics(),
    )
    .await?;

    let mut granted = false;
    if args.grant_everyone {
        match images::manageable_source(&added.image, &args.registry, &args.repository) {
            Some(source) => {
                images::grant_everyone(&client, &digest, source).await?;
                granted = true;
            }
            // The add was a no-op: someone else registered this digest from
            // the same repository first, and their grants are the ones in
            // force. Saying so is better than a grant that silently did not
            // happen.
            None => ctx.warn(&format!(
                "{}@{digest} is sourced by another owner; not granting",
                args.repository,
            )),
        }
    }

    ok(
        ctx,
        &format!(
            "{} {}@{digest}{}",
            if added.registered {
                "Registered"
            } else {
                "Already known:"
            },
            args.repository,
            if granted { " · public" } else { "" },
        ),
    );
    ctx.result(Event::ImageRegistered {
        digest,
        registry: &args.registry,
        repository: &args.repository,
        registered: added.registered,
        granted,
    });
    Ok(())
}

/// `tml image publish`: register everything built, then roll the sets.
pub async fn publish(ctx: &Ctx, args: &ImagePublishArgs) -> Result<()> {
    let config = read_config(&args.config)?;
    let client = ctx.authenticated_client()?;

    if let Some(configured) = &config.switchboard {
        let session = &ctx.session.config.switchboard;
        if !same_switchboard(configured, session) {
            bail!(
                "{} publishes to {configured}, but this profile is pointed at {session}. \
                 Publishing to the wrong switchboard is not easily undone; \
                 pass --switchboard {configured}, or switch profile.",
                args.config.display(),
            );
        }
    }

    let records = read_records(&args.records)?;
    if records.is_empty() {
        ctx.note("No images were pushed; nothing to publish");
        return Ok(());
    }
    if let Some(expected) = &config.registry {
        for record in &records {
            if &record.registry != expected {
                // Not fatal — a mirror is legitimate — but a record from an
                // unexpected registry is usually a misrouted push.
                ctx.warn(&format!(
                    "{} was pushed to {}, but {} expects {expected}",
                    record.image,
                    record.registry,
                    args.config.display(),
                ));
            }
        }
    }

    let me = client.whoami().await.context("identifying this session")?;
    ctx.note(&format!(
        "Publishing as {} ({}){}",
        me.name,
        me.user_id,
        if args.dry_run { ", dry run" } else { "" },
    ));

    // -- make every pushed image known and public ----------------------

    for record in &records {
        if args.dry_run {
            ctx.note(&format!(
                "Would register {}/{}@{}",
                record.registry, record.repository, record.digest,
            ));
            continue;
        }
        let added = images::register(
            &client,
            &record.digest,
            &record.registry,
            &record.repository,
            timeout(args.timeout),
            ctx.diagnostics(),
        )
        .await?;
        match images::manageable_source(&added.image, &record.registry, &record.repository) {
            Some(source) => images::grant_everyone(&client, &record.digest, source).await?,
            None => ctx.warn(&format!(
                "{}@{} is sourced by another owner; not granting",
                record.repository, record.digest,
            )),
        }
        ctx.detail(&format!(
            "registered {}/{}@{}",
            record.registry, record.repository, record.digest,
        ));
    }

    // -- roll each set forward ------------------------------------------

    let sets = client
        .list_image_sets()
        .await
        .context("listing the image sets")?;
    let digests: BTreeMap<&str, _> = records
        .iter()
        .map(|record| (record.image.as_str(), record.digest))
        .collect();

    for (name, members) in &config.sets {
        let set = images::find_set(&sets, name).with_context(|| {
            format!(
                "no image set with canonical name {name} is visible to {}",
                me.name
            )
        })?;

        let current = match set.latest_generation {
            Some(generation) => {
                client
                    .image_set_generation(set.id, generation)
                    .await
                    .with_context(|| format!("reading generation {generation} of {name}"))?
                    .members
            }
            None => Vec::new(),
        };

        let desired: Vec<images::DesiredMember> = members
            .iter()
            .map(|member| images::DesiredMember {
                platform_profile: member.platform_profile.clone(),
                predicate: member.predicate.clone(),
                manifest_digest: digests.get(member.image.as_str()).copied(),
            })
            .collect();

        let Some(planned) = images::plan_generation(&current, &desired)
            .with_context(|| format!("planning the next generation of {name}"))?
        else {
            ctx.note(&format!(
                "{name} (generation {}) is unchanged",
                set.latest_generation
                    .map(|g| g.to_string())
                    .unwrap_or_else(|| "none".into()),
            ));
            ctx.event(Event::ImageSetUnchanged {
                set: name,
                generation: set.latest_generation,
            });
            continue;
        };

        if args.dry_run {
            ctx.note(&format!(
                "Would append {} member{} to {name}: {}",
                planned.len(),
                if planned.len() == 1 { "" } else { "s" },
                describe(members, &planned),
            ));
            continue;
        }

        let generation = client
            .create_image_set_generation(
                set.id,
                &treadmill_rs::api::switchboard::images::CreateGenerationRequest {
                    members: planned,
                },
            )
            .await
            .with_context(|| format!("appending a generation to {name}"))?;

        // The health check that proves the grants above actually landed: a
        // set nobody can pull from is grantable but useless.
        let unusable = images::unusable_members(&generation);
        if !unusable.is_empty() {
            let digests: Vec<String> = unusable
                .iter()
                .map(|member| member.manifest_digest.to_string())
                .collect();
            bail!(
                "{name} generation {} has members not every grantee can use: {}",
                generation.generation,
                digests.join(", "),
            );
        }

        ok(
            ctx,
            &format!(
                "{name} · generation {} · {} member{}",
                generation.generation,
                generation.members.len(),
                if generation.members.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ),
        );
        ctx.event(Event::ImageSetRolled {
            set: name,
            generation: generation.generation,
            members: generation.members.len(),
        });
    }

    Ok(())
}

/// Read and sanity-check a publish configuration.
fn read_config(path: &Path) -> Result<Config> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let config: Config = serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    for (name, members) in &config.sets {
        if members.is_empty() {
            bail!("{}: set {name} has no members", path.display());
        }
        for member in members {
            if member.platform_profile.is_empty() {
                bail!(
                    "{}: a member of {name} has an empty platform_profile",
                    path.display(),
                );
            }
        }
    }
    Ok(config)
}

/// Whether two switchboard URLs name the same place, ignoring a trailing
/// slash.
fn same_switchboard(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The members of a planned generation, named by image rather than digest.
fn describe(configured: &[ConfigMember], planned: &[GenerationMemberSpec]) -> String {
    configured
        .iter()
        .zip(planned)
        .map(|(member, spec)| {
            Member {
                image: member.image.clone(),
                spec: spec.clone(),
            }
            .label()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_slash_does_not_make_it_a_different_switchboard() {
        assert!(same_switchboard(
            "https://swb.treadmill.dev",
            "https://swb.treadmill.dev/"
        ));
        assert!(!same_switchboard(
            "https://swb.treadmill.dev",
            "https://staging.treadmill.dev"
        ));
    }

    fn config(json: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publish.json");
        std::fs::write(&path, json).unwrap();
        read_config(&path)
    }

    #[test]
    fn a_publish_configuration_parses() {
        let parsed = config(
            r#"{
                "switchboard": "https://swb.treadmill.dev",
                "registry": "ghcr.io:443",
                "sets": {
                    "linux": [
                        {"image": "ubuntu-server-2604/tock",
                         "platform_profile": "q35-virtio-uefi",
                         "predicate": null},
                        {"image": "raspberrypios-13/tock",
                         "platform_profile": "arm64-nbd-netboot",
                         "predicate": "host.platform.vendor == 'Raspberry Pi'"}
                    ]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.registry.as_deref(), Some("ghcr.io:443"));
        let linux = &parsed.sets["linux"];
        assert_eq!(linux.len(), 2);
        assert_eq!(linux[0].image, "ubuntu-server-2604/tock");
        assert_eq!(
            linux[1].predicate.as_deref(),
            Some("host.platform.vendor == 'Raspberry Pi'")
        );
    }

    #[test]
    fn an_empty_set_is_refused() {
        // A set with no members would publish an image set nothing can run.
        let error = config(r#"{"registry": "ghcr.io:443", "sets": {"linux": []}}"#).unwrap_err();
        assert!(format!("{error:#}").contains("no members"), "{error:#}");
    }

    #[test]
    fn a_member_without_a_platform_is_refused() {
        let error =
            config(r#"{"sets": {"linux": [{"image": "a", "platform_profile": ""}]}}"#).unwrap_err();
        assert!(
            format!("{error:#}").contains("platform_profile"),
            "{error:#}"
        );
    }
}
