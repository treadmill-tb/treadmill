# Assemble image-group members into a self-contained Treadmill OCI image-index
# layout — the producer-side counterpart of `treadmill_rs::image::parse_group`.
#
# A group is an OCI image index (`artifactType = image-group.v1+json`) whose
# manifest descriptors reference the concrete per-image manifests BY DIGEST and
# carry the OCI `platform` plus the `ci.treadmill.required-host-tags` scheduling
# policy (a comma-separated tag list; see annotations.rs `parse_tag_list`).
# `required-host-tags` lives here, in the group, and is NEVER baked into a
# per-image manifest or root filesystem (doc/images-oci-migration-plan.md §I6).
#
# The layout is SELF-CONTAINED: every member's manifest, config and layer blobs
# are copied in, so a single `skopeo copy oci:<layout> docker://…` pushes the
# index and all referenced member content in one shot.
{
  pkgs,
  lib,
  mediaTypes,
}:
{
  name, # e.g. "ubuntu-2204"
  # List of { image; platform = { architecture; os; variant ? null; };
  #           requiredHostTags = [ "arch=amd64" ... ]; }
  members,
}:
let
  inherit (mediaTypes) ann;

  memberBlock =
    member:
    let
      platform = lib.filterAttrs (_: v: v != null) member.platform;
      tagCsv = lib.concatStringsSep ", " member.requiredHostTags;
    in
    ''
      # --- group member: ${member.image.name or "image"} ---
      img=${lib.escapeShellArg member.image}
      # Member's manifest descriptor (digest + size) off its own index.json.
      mdigest="$(jq -r '.manifests[0].digest' "$img/index.json")"
      msize="$(jq -r '.manifests[0].size' "$img/index.json")"
      # Copy the member's entire blob set into the group's CAS (idempotent;
      # shared base blobs across members dedupe by digest).
      cp -n "$img"/blobs/sha256/* "$blobs/" 2>/dev/null || true

      member_desc="$(jq -n \
        --arg mt ${lib.escapeShellArg mediaTypes.ociManifest} \
        --arg at ${lib.escapeShellArg mediaTypes.imageArtifactType} \
        --arg dg "$mdigest" --argjson sz "$msize" \
        --argjson platform '${builtins.toJSON platform}' \
        --arg tags ${lib.escapeShellArg tagCsv} \
        '{
          mediaType: $mt,
          artifactType: $at,
          digest: $dg,
          size: $sz,
          platform: $platform,
          annotations: { ${builtins.toJSON ann.requiredHostTags}: $tags }
        }')"
      members_json="$(printf '%s' "$members_json" | jq --argjson m "$member_desc" '. + [$m]')"
    '';
in
pkgs.runCommand "treadmill-group-${name}"
  {
    nativeBuildInputs = [
      pkgs.jq
      pkgs.coreutils
    ];
    passthru = {
      groupName = name;
    };
  }
  ''
    set -euo pipefail

    blobs="$out/blobs/sha256"
    mkdir -p "$blobs"

    members_json='[]'

    ${lib.concatMapStrings memberBlock members}

    printf '{"imageLayoutVersion":"1.0.0"}' > "$out/oci-layout"

    jq -n --argjson members "$members_json" \
      '{
        schemaVersion: 2,
        mediaType: ${builtins.toJSON mediaTypes.ociIndex},
        artifactType: ${builtins.toJSON mediaTypes.groupArtifactType},
        manifests: $members
      }' > "$out/index.json"
  ''
