# Declarative image-group membership + scheduling policy (plan §5, §I4–I6).
#
# A group is an OCI image index whose members reference the concrete per-image
# manifests by digest and carry the OCI `platform` plus the
# `ci.treadmill.required-host-tags` scheduling policy. `required-host-tags`
# lives HERE (never in a per-image manifest or root filesystem): it is the
# host-selection policy the scheduler applies, declared once, in one table.
#
# "with runner" is an explicit user choice rather than a host-derived variant,
# so each runner image is its own logical group (plan I4). Every group has
# exactly one member today; adding e.g. an `ubuntu-arm64-qemu` host variant
# later is a one-line addition to the relevant member list (plan I5) — this is
# the single extension point for future members.
#
# `nix/images.nix` feeds the built `mkTreadmillImage` layouts in as `images` and
# maps each entry through `images/lib/mk-group-index.nix` -> `group-<name>`.
{ images }: # the built mkTreadmillImage layouts, keyed by image name
{
  ubuntu-2204 = [
    {
      image = images.ubuntu-2204;
      platform = {
        architecture = "amd64";
        os = "linux";
      };
      requiredHostTags = [ "arch=amd64" ];
    }
  ];
  ubuntu-2204-gha-runner = [
    {
      image = images.ubuntu-2204-gha-runner;
      platform = {
        architecture = "amd64";
        os = "linux";
      };
      requiredHostTags = [ "arch=amd64" ];
    }
  ];
  raspbian-13 = [
    {
      image = images.raspbian-13;
      platform = {
        architecture = "arm64";
        os = "linux";
        variant = "v8";
      };
      requiredHostTags = [
        "arch=arm64"
        "raspberrypi"
      ];
    }
  ];
  raspbian-13-gha-runner = [
    {
      image = images.raspbian-13-gha-runner;
      platform = {
        architecture = "arm64";
        os = "linux";
        variant = "v8";
      };
      requiredHostTags = [
        "arch=arm64"
        "raspberrypi"
      ];
    }
  ];
}
