//! Moving a layout between local disk and an OCI registry.
//!
//! A Treadmill image is a *pure artifact*: a manifest with a custom
//! `artifactType`, an empty config, and layers whose media types and
//! annotations are this project's own. That rules out `oci-client`'s
//! convenience `push`, which models a conventional runnable image and would
//! rewrite the manifest into something else.
//!
//! So this works one level down. Blobs are streamed by digest, and the
//! manifest is pushed as the **exact bytes** the layout holds. The digest is
//! therefore preserved by construction, which is what the switchboard
//! registration downstream depends on: it records a digest, and a digest that
//! changed in transit refers to nothing.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use futures_util::TryStreamExt as _;
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::manifest::OciDescriptor;
use oci_client::secrets::RegistryAuth;
use oci_client::{Reference, RegistryOperation};
use serde::{Deserialize, Serialize};

use treadmill_rs::image::{Digest, media_types};

use crate::layout::Layout;

/// What a push leaves behind for the publish step.
///
/// The asymmetry in here is deliberate and load-bearing: `registry` is the
/// authority *with* its port, because that is the form the switchboard's
/// catalog records, while the push itself goes to the host as written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRecord {
    /// The image this was built from, as a coordinate.
    pub image: String,
    /// Registry authority, `host:port`, as the catalog records it.
    pub registry: String,
    /// Repository path within the registry.
    pub repository: String,
    /// The manifest digest, unchanged by the push.
    pub digest: Digest,
}

/// Where to push, and as whom.
#[derive(Debug, Clone)]
pub struct Target {
    /// The registry host as the push addresses it, e.g. `ghcr.io`.
    pub registry: String,
    /// The repository path, e.g. `treadmill-tb/ubuntu-server-2604`.
    pub repository: String,
    /// The tags to publish under. The digest is pushed regardless.
    pub tags: Vec<String>,
    /// Talk plain HTTP. For a local registry, never for a real one.
    pub insecure: bool,
}

/// Push every blob of `layout`, then its manifest, under each of
/// `target.tags`.
pub fn push(layout_dir: &Path, image: &str, target: &Target) -> Result<PushRecord> {
    let layout = Layout::open(layout_dir)?;
    let (digest, manifest) = layout.manifest()?;

    // The bytes as they are on disk. Re-serializing would be a different
    // document and so a different digest.
    let manifest_bytes = std::fs::read(layout.blob_path(&digest))
        .with_context(|| format!("read the manifest of {}", layout_dir.display()))?;

    let auth = credentials(&target.registry)?;
    let runtime = runtime()?;

    runtime.block_on(async {
        let client = client(target.insecure);
        let reference = reference_for(target, &digest.to_string())?;
        client
            .auth(&reference, &auth, RegistryOperation::Push)
            .await
            .with_context(|| format!("authenticate to {}", target.registry))?;

        // The config blob is as much a blob as the layers, and a registry
        // rejects a manifest whose config it does not already hold.
        let mut blobs = vec![descriptor_of(manifest.config())];
        blobs.extend(manifest.layers().iter().map(descriptor_of));

        for blob in &blobs {
            push_blob(&client, &reference, &layout, blob).await?;
        }

        let content_type: http::HeaderValue = media_types::oci::MANIFEST
            .parse()
            .expect("a static media type is a valid header value");
        // Under the digest first, so the content exists before any tag
        // points at it.
        for name in std::iter::once(digest.to_string()).chain(target.tags.iter().cloned()) {
            let reference = reference_for(target, &name)?;
            client
                .push_manifest_raw(&reference, manifest_bytes.clone(), content_type.clone())
                .await
                .with_context(|| format!("push the manifest as {name}"))?;
        }
        anyhow::Ok(())
    })?;

    Ok(PushRecord {
        image: image.to_string(),
        registry: authority(&target.registry),
        repository: target.repository.clone(),
        digest,
    })
}

/// Pull `reference` into a fresh layout at `out`.
///
/// This is what `--lower-ref` resolves: an image built elsewhere, used as the
/// base of one built here.
pub fn pull(reference: &str, out: &Path, insecure: bool) -> Result<()> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("parse the image reference {reference:?}"))?;
    let auth = credentials(parsed.registry())?;

    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    let layout = Layout::create(out)?;

    let runtime = runtime()?;
    runtime.block_on(async {
        let client = client(insecure);
        let (manifest_bytes, manifest_digest) = client
            .pull_manifest_raw(
                &parsed,
                &auth,
                &[media_types::oci::MANIFEST, media_types::IMAGE_ARTIFACT_TYPE],
            )
            .await
            .with_context(|| format!("pull the manifest of {reference}"))?;

        let manifest: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&manifest_bytes)
                .with_context(|| format!("parse the manifest of {reference}"))?;

        let mut blobs = vec![manifest.config.clone()];
        blobs.extend(manifest.layers.iter().cloned());
        for blob in &blobs {
            pull_blob(&client, &parsed, &layout, blob).await?;
        }

        // Stored verbatim, so the layout's manifest digest is the one the
        // registry served.
        let digest: Digest = manifest_digest
            .parse()
            .with_context(|| format!("{reference} served a malformed digest"))?;
        layout.store_manifest_bytes(&digest, &manifest_bytes)?;
        anyhow::Ok(())
    })?;
    Ok(())
}

/// Stream one blob up, skipping it if the registry already has it.
async fn push_blob(
    client: &Client,
    reference: &Reference,
    layout: &Layout,
    descriptor: &OciDescriptor,
) -> Result<()> {
    use tokio_util::io::ReaderStream;

    let digest: Digest = descriptor
        .digest
        .parse()
        .with_context(|| format!("layout holds a malformed digest {}", descriptor.digest))?;
    let path = layout.blob_path(&digest);

    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let size = file
        .metadata()
        .await
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let stream = ReaderStream::new(file)
        .map_err(|e| oci_client::errors::OciDistributionError::GenericError(Some(e.to_string())));

    client
        .push_blob_stream(reference, stream, &descriptor.digest, Some(size))
        .await
        .with_context(|| format!("push blob {}", descriptor.digest))?;
    Ok(())
}

/// Pull one blob down into the layout.
async fn pull_blob(
    client: &Client,
    reference: &Reference,
    layout: &Layout,
    descriptor: &OciDescriptor,
) -> Result<()> {
    let digest: Digest = descriptor
        .digest
        .parse()
        .with_context(|| format!("{reference} names a malformed digest {}", descriptor.digest))?;
    let path = layout.blob_path(&digest);
    if path.exists() {
        return Ok(());
    }

    let partial = path.with_extension("partial");
    let file = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("create {}", partial.display()))?;
    client
        .pull_blob(reference, descriptor, file)
        .await
        .with_context(|| format!("pull blob {}", descriptor.digest))?;
    // Renamed only once complete, so an interrupted pull cannot leave a
    // truncated blob looking like a valid one.
    std::fs::rename(&partial, &path)
        .with_context(|| format!("move {} into place", partial.display()))?;
    Ok(())
}

/// `oci-spec`'s descriptor as `oci-client`'s.
///
/// The two crates model the same thing and do not know about each other. Only
/// the three fields a blob upload needs are carried across; annotations and
/// URLs describe the manifest, which is pushed verbatim and so does not go
/// through here.
fn descriptor_of(descriptor: &oci_spec::image::Descriptor) -> OciDescriptor {
    OciDescriptor {
        media_type: descriptor.media_type().to_string(),
        digest: descriptor.digest().to_string(),
        size: descriptor.size() as i64,
        ..Default::default()
    }
}

/// A reference to `name`, which is either a tag or a digest.
fn reference_for(target: &Target, name: &str) -> Result<Reference> {
    let text = if name.starts_with("sha256:") {
        format!("{}/{}@{name}", target.registry, target.repository)
    } else {
        format!("{}/{}:{name}", target.registry, target.repository)
    };
    text.parse()
        .with_context(|| format!("construct a reference to {text}"))
}

fn client(insecure: bool) -> Client {
    Client::new(ClientConfig {
        protocol: if insecure {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        ..Default::default()
    })
}

/// A runtime for the one async thing this crate does.
///
/// The build is synchronous throughout; this is confined to the two functions
/// that talk to a registry. A caller already inside a runtime has to reach
/// these from a blocking context.
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start a runtime for the registry client")
}

/// `host:port`, defaulting to the HTTPS port.
///
/// The catalog identifies a source by an authority that carries its port, so
/// `ghcr.io` and `ghcr.io:443` must not become two different sources.
fn authority(registry: &str) -> String {
    if registry.contains(':') {
        registry.to_string()
    } else {
        format!("{registry}:443")
    }
}

/// The docker-style credential store.
#[derive(Debug, Deserialize, Default)]
struct DockerConfig {
    #[serde(default)]
    auths: std::collections::BTreeMap<String, DockerAuth>,
}

#[derive(Debug, Deserialize, Default)]
struct DockerAuth {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Credentials for `registry`, from the environment or a credential store.
///
/// `TML_REGISTRY_USERNAME`/`TML_REGISTRY_PASSWORD` win, so CI can pass a token
/// without writing a file. Otherwise this reads the stores the tools a user
/// already has would have written, in the order those tools look:
/// `$REGISTRY_AUTH_FILE`, then the `containers/auth.json` that `skopeo login`
/// and `podman login` write, then `~/.docker/config.json`.
///
/// Reading only the docker config was a real bug: `skopeo login ghcr.io` puts
/// nothing there — it writes `$XDG_RUNTIME_DIR/containers/auth.json` — so a
/// push failed to authenticate for someone who was, as far as they could
/// tell, logged in.
pub fn credentials(registry: &str) -> Result<RegistryAuth> {
    if let (Ok(username), Ok(password)) = (
        std::env::var("TML_REGISTRY_USERNAME"),
        std::env::var("TML_REGISTRY_PASSWORD"),
    ) && !username.is_empty()
    {
        return Ok(RegistryAuth::Basic(username, password));
    }

    for path in credential_files() {
        if !path.is_file() {
            continue;
        }
        if let Some(auth) = credentials_in(&path, registry)? {
            return Ok(auth);
        }
    }
    Ok(RegistryAuth::Anonymous)
}

/// The credential stores to consult, most specific first.
fn credential_files() -> Vec<PathBuf> {
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    credential_files_from(
        var("REGISTRY_AUTH_FILE"),
        var("XDG_RUNTIME_DIR"),
        var("HOME"),
        var("DOCKER_CONFIG"),
    )
}

/// [`credential_files`] with the environment passed in, so the order can be
/// asserted without mutating process-global state.
fn credential_files_from(
    auth_file: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    docker_config: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(file) = auth_file {
        paths.push(file);
    }
    if let Some(dir) = runtime_dir {
        paths.push(dir.join("containers/auth.json"));
    }
    if let Some(home) = &home {
        paths.push(home.join(".config/containers/auth.json"));
    }
    match (docker_config, home) {
        (Some(dir), _) => paths.push(dir.join("config.json")),
        (None, Some(home)) => paths.push(home.join(".docker/config.json")),
        (None, None) => {}
    }
    paths
}

/// What `path` holds for `registry`, if anything.
///
/// Registries are keyed inconsistently across tools: bare host, host with a
/// port, and the legacy full URL all occur.
fn credentials_in(path: &Path, registry: &str) -> Result<Option<RegistryAuth>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let config: DockerConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;

    let candidates = [
        registry.to_string(),
        authority(registry),
        registry.split(':').next().unwrap_or(registry).to_string(),
        format!("https://{registry}"),
    ];
    for candidate in candidates {
        let Some(entry) = config.auths.get(&candidate) else {
            continue;
        };
        if let (Some(username), Some(password)) = (&entry.username, &entry.password) {
            return Ok(Some(RegistryAuth::Basic(
                username.clone(),
                password.clone(),
            )));
        }
        if let Some(encoded) = &entry.auth {
            return decode_auth(encoded)
                .map(Some)
                .with_context(|| format!("{}: credentials for {candidate}", path.display()));
        }
    }
    Ok(None)
}

/// Decode a docker `auth` field: base64 of `username:password`.
fn decode_auth(encoded: &str) -> Result<RegistryAuth> {
    use base64::Engine as _;

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("the auth field is not base64")?;
    let decoded = String::from_utf8(decoded).context("the auth field is not UTF-8")?;
    let (username, password) = decoded
        .split_once(':')
        .context("the auth field is not username:password")?;
    if username.is_empty() {
        bail!("the auth field has no username");
    }
    Ok(RegistryAuth::Basic(
        username.to_string(),
        password.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(registry: &str) -> Target {
        Target {
            registry: registry.to_string(),
            repository: "org/image".to_string(),
            tags: vec!["latest".to_string()],
            insecure: false,
        }
    }

    #[test]
    fn the_recorded_authority_always_carries_a_port() {
        // The catalog keys sources by authority; ghcr.io and ghcr.io:443 must
        // not become two sources for the same image.
        assert_eq!(authority("ghcr.io"), "ghcr.io:443");
        assert_eq!(authority("ghcr.io:443"), "ghcr.io:443");
        assert_eq!(authority("localhost:5000"), "localhost:5000");
    }

    #[test]
    fn a_digest_is_referenced_with_an_at_sign_and_a_tag_with_a_colon() {
        let target = target("ghcr.io");
        let digest = format!("sha256:{}", "ab".repeat(32));
        let by_digest = reference_for(&target, &digest).unwrap();
        assert_eq!(by_digest.digest(), Some(digest.as_str()));
        assert_eq!(by_digest.repository(), "org/image");

        let by_tag = reference_for(&target, "v1").unwrap();
        assert_eq!(by_tag.tag(), Some("v1"));
    }

    #[test]
    fn docker_credentials_decode_from_the_auth_field() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:secret");
        match decode_auth(&encoded).unwrap() {
            RegistryAuth::Basic(username, password) => {
                assert_eq!(username, "user");
                assert_eq!(password, "secret");
            }
            other => panic!("expected basic credentials, got {other:?}"),
        }
    }

    #[test]
    fn a_password_containing_a_colon_survives() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:a:b:c");
        match decode_auth(&encoded).unwrap() {
            RegistryAuth::Basic(_, password) => assert_eq!(password, "a:b:c"),
            other => panic!("expected basic credentials, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_auth_field_is_refused() {
        assert!(decode_auth("not base64!!").is_err());
        use base64::Engine as _;
        let no_colon = base64::engine::general_purpose::STANDARD.encode("nouser");
        assert!(decode_auth(&no_colon).is_err());
    }

    #[test]
    fn the_containers_store_is_consulted_before_the_docker_one() {
        let files = credential_files_from(
            None,
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/someone")),
            None,
        );
        let shown: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            shown,
            [
                "/run/user/1000/containers/auth.json",
                "/home/someone/.config/containers/auth.json",
                "/home/someone/.docker/config.json",
            ]
        );
    }

    #[test]
    fn an_explicit_auth_file_wins() {
        let files = credential_files_from(
            Some(PathBuf::from("/explicit/auth.json")),
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/someone")),
            Some(PathBuf::from("/docker")),
        );
        assert_eq!(files.first().unwrap(), Path::new("/explicit/auth.json"));
        assert_eq!(files.last().unwrap(), Path::new("/docker/config.json"));
    }

    #[test]
    fn a_store_that_knows_nothing_about_the_registry_claims_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        std::fs::write(&auth, r#"{"auths": {}}"#).unwrap();
        assert!(credentials_in(&auth, "ghcr.io").unwrap().is_none());
    }

    #[test]
    fn a_containers_auth_file_is_read_as_well_as_the_docker_one() {
        // `skopeo login` and `podman login` write containers/auth.json, not
        // ~/.docker/config.json. Reading only the latter meant a push failed
        // to authenticate for someone who was, as far as they could tell,
        // logged in.
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let encoded = base64::engine::general_purpose::STANDARD.encode("alevy:token");
        std::fs::write(
            &auth,
            format!(r#"{{"auths": {{"ghcr.io": {{"auth": "{encoded}"}}}}}}"#),
        )
        .unwrap();

        match credentials_in(&auth, "ghcr.io").unwrap() {
            Some(RegistryAuth::Basic(username, password)) => {
                assert_eq!(username, "alevy");
                assert_eq!(password, "token");
            }
            other => panic!("expected credentials, got {other:?}"),
        }
        // And a registry it says nothing about is not claimed.
        assert!(credentials_in(&auth, "registry.example").unwrap().is_none());
    }
}
