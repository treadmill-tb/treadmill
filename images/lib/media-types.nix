# Nix mirror of treadmill-rs `image/media_types.rs` + `image/annotations.rs`.
#
# These string literals are the ONLY copy of the media types / artifact types /
# annotation keys on the producer side. Correctness against the consumer (the
# `treadmill_rs::image::parse` view) is enforced by the §6 per-image parse-check
# (the `image-check` binary, run via the `image-check-*` derivations), not by
# hoping these match. If a literal drifts from the Rust constant, that check is
# what catches it.
{
  imageArtifactType = "application/vnd.treadmill.image.v1+json";
  diskQcow2 = "application/vnd.treadmill.disk.qcow2";
  bootFatV1 = "application/vnd.treadmill.boot.fat.v1";
  ociEmptyConfig = "application/vnd.oci.empty.v1+json";
  ociManifest = "application/vnd.oci.image.manifest.v1+json";
  ociIndex = "application/vnd.oci.image.index.v1+json";
  ann = {
    role = "ci.treadmill.role";
    head = "ci.treadmill.qcow2.head";
    lower = "ci.treadmill.qcow2.lower";
    virtualSize = "ci.treadmill.qcow2.virtual-size";
    requiredHostTags = "ci.treadmill.required-host-tags";
    title = "org.opencontainers.image.title";
    version = "org.opencontainers.image.version";
    description = "org.opencontainers.image.description";
    baseName = "org.opencontainers.image.base.name";
  };
}
