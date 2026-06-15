# Assemble an ordered list of blob layers into a validated Treadmill OCI image
# layout — the producer-side counterpart of `treadmill_rs::image::parse`.
#
# This is the parameterized generalization of the hand-assembly in
# `nix/tiny-efi.nix`: an empty-config pure-artifact manifest carrying the
# Treadmill media types + `ci.treadmill.*` annotations, wrapped in a single
# top-level OCI index. One helper produces every shape we build: a 1-root-layer
# image (ubuntu base), a 2-root-layer backing chain (runner overlays), and a
# boot-FAT + root image (raspbian netboot).
#
# Chain wiring is derived from layer ORDER + role (matches parse.rs):
#   * Consider the sub-sequence of `role = "root"` layers, in order. The first
#     gets no `ci.treadmill.qcow2.lower`; each subsequent root layer's `lower`
#     is the digest of the previous root layer. The manifest-level
#     `ci.treadmill.qcow2.head` is the digest of the LAST root layer.
#   * `role = "boot"` layers are standalone: no `lower`, never `head`, and no
#     `qcow2.virtual-size` (a raw FAT's size is the descriptor `size`; see
#     doc/images-oci-migration-plan.md §4.3).
{
  pkgs,
  lib,
  mediaTypes,
}:
{
  name, # store-path label, e.g. "ubuntu-2204"
  title, # org.opencontainers.image.title
  version ? null,
  description ? null,
  # ORDERED list of { path; mediaType; role; virtualSize ? null; }. `virtualSize`
  # is only meaningful for qcow2 root layers; if null it is computed with
  # `qemu-img info` for root layers and omitted for boot layers.
  layers,
}:
let
  inherit (mediaTypes) ann;

  # Emit the bash that stores one layer blob, computes its annotations (role,
  # virtual-size for qcow2 roots, lower for chained roots), and appends the
  # descriptor to `$layers_json`, tracking the running root-chain state.
  layerBlock =
    layer:
    let
      isRoot = layer.role == "root";
      vsExpr =
        if layer.virtualSize != null then
          "vs=${lib.escapeShellArg (toString layer.virtualSize)}"
        else
          ''vs="$(qemu-img info --output=json "$lpath" | jq -r '."virtual-size"')"'';
    in
    ''
      # --- layer: role=${layer.role} mediaType=${layer.mediaType} ---
      lpath=${lib.escapeShellArg layer.path}
      ldigest="$(store "$lpath")"
      lsize="$(size "$lpath")"
      lann="$(jq -n --arg role ${lib.escapeShellArg layer.role} \
        '{ ${builtins.toJSON ann.role}: $role }')"
    ''
    + lib.optionalString isRoot ''
      ${vsExpr}
      lann="$(printf '%s' "$lann" | jq --arg vs "$vs" \
        '. + { ${builtins.toJSON ann.virtualSize}: $vs }')"
      if [ -n "$prev_root" ]; then
        lann="$(printf '%s' "$lann" | jq --arg lower "$prev_root" \
          '. + { ${builtins.toJSON ann.lower}: $lower }')"
      fi
      prev_root="sha256:$ldigest"
      head_digest="sha256:$ldigest"
    ''
    + ''
      layer_obj="$(jq -n \
        --arg mt ${lib.escapeShellArg layer.mediaType} \
        --arg dg "sha256:$ldigest" \
        --argjson sz "$lsize" \
        --argjson ann "$lann" \
        '{ mediaType: $mt, digest: $dg, size: $sz, annotations: $ann }')"
      layers_json="$(printf '%s' "$layers_json" | jq --argjson l "$layer_obj" '. + [$l]')"
    '';
in
pkgs.runCommand "treadmill-image-${name}"
  {
    nativeBuildInputs = [
      pkgs.jq
      pkgs.qemu-utils
      pkgs.coreutils
    ];
    passthru = { inherit title; };
  }
  ''
    set -euo pipefail

    blobs="$out/blobs/sha256"
    mkdir -p "$blobs"

    # store <file> -> echoes the sha256 hex and copies the file into the CAS
    # (content-addressed, so a repeated identical blob is stored once).
    store() {
      local f="$1" d
      d="$(sha256sum "$f" | cut -d' ' -f1)"
      [ -e "$blobs/$d" ] || cp "$f" "$blobs/$d"
      printf '%s' "$d"
    }
    size() { stat -c%s "$1"; }

    # Empty config blob marks the manifest as a pure artifact.
    printf '{}' > config.json
    cfg_digest="$(store config.json)"
    cfg_size="$(size config.json)"

    layers_json='[]'
    prev_root=""    # "sha256:<hex>" of the previous root layer (chain wiring)
    head_digest=""  # "sha256:<hex>" of the last root layer

    ${lib.concatMapStrings layerBlock layers}

    if [ -z "$head_digest" ]; then
      echo "mkTreadmillImage(${name}): image has no role=root layer" >&2
      exit 1
    fi

    manifest_ann="$(jq -n --arg head "$head_digest" --arg title ${lib.escapeShellArg title} \
      '{ ${builtins.toJSON ann.head}: $head, ${builtins.toJSON ann.title}: $title }')"
    ${lib.optionalString (version != null) ''
      manifest_ann="$(printf '%s' "$manifest_ann" | jq --arg v ${lib.escapeShellArg version} \
        '. + { ${builtins.toJSON ann.version}: $v }')"
    ''}
    ${lib.optionalString (description != null) ''
      manifest_ann="$(printf '%s' "$manifest_ann" | jq --arg d ${lib.escapeShellArg description} \
        '. + { ${builtins.toJSON ann.description}: $d }')"
    ''}

    jq -n \
      --arg cfg "sha256:$cfg_digest" --argjson cfgsize "$cfg_size" \
      --argjson layers "$layers_json" --argjson ann "$manifest_ann" \
      '{
        schemaVersion: 2,
        mediaType: ${builtins.toJSON mediaTypes.ociManifest},
        artifactType: ${builtins.toJSON mediaTypes.imageArtifactType},
        config: {
          mediaType: ${builtins.toJSON mediaTypes.ociEmptyConfig},
          digest: $cfg,
          size: $cfgsize,
          data: "e30="
        },
        layers: $layers,
        annotations: $ann
      }' > manifest.json

    manifest_digest="$(store manifest.json)"
    manifest_size="$(size manifest.json)"

    printf '{"imageLayoutVersion":"1.0.0"}' > "$out/oci-layout"

    jq -n \
      --arg manifest "sha256:$manifest_digest" --argjson manifestsize "$manifest_size" \
      '{
        schemaVersion: 2,
        mediaType: ${builtins.toJSON mediaTypes.ociIndex},
        manifests: [
          {
            mediaType: ${builtins.toJSON mediaTypes.ociManifest},
            artifactType: ${builtins.toJSON mediaTypes.imageArtifactType},
            digest: $manifest,
            size: $manifestsize
          }
        ]
      }' > "$out/index.json"
  ''
