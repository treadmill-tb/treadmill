{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      system,
      ...
    }:
    let
      cmn = import ./lib.nix { inherit inputs system pkgs; };

      defaultShell = cmn.craneLib.devShell {
        packages = with pkgs; [
          pkg-config
          openssl
          sqlx-cli
          nixfmt
          statix
          deadnix
          taplo
          cargo-audit
          cargo-nextest
          cargo-outdated
          postgresql
          sql-formatter

          # OCI image-migration tooling: the vendored Zot registry (per-server
          # store daemon / pull-through cache) plus skopeo for moving images
          # between OCI layouts and registries in tests and the CLI.
          cmn.zot
          skopeo

          # qemu provides qemu-img (backing-chain validation) and
          # qemu-system-aarch64 + AAVMF firmware (the tiny-efi boot test).
          qemu

          # Log streaming: the NATS server + JetStream (`nats run .#dev` runs a
          # live broker), `nsc` to bootstrap the decentralized-JWT auth
          # hierarchy, and the `nats` CLI for ad-hoc pub/sub against it. Note:
          # nats-server binds a TCP/WebSocket port, so it cannot run in the
          # restricted sandbox (see AGENTS.md §2) — verify NATS-touching code via
          # its hermetic Nix check, not by running the daemon by hand. `nsc`
          # itself only writes files and runs fine in the sandbox.
          nats-server
          nsc
          natscli
        ];

        shellHook = ''
          # AAVMF (aarch64 UEFI) firmware for the tiny-efi qemu boot test. The
          # qemu package ships the code + variable-store templates under
          # share/qemu; the boot test can also locate them next to the binary,
          # but exporting them keeps it explicit and overridable.
          export TML_AAVMF_CODE="${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"
          export TML_AAVMF_VARS="${pkgs.qemu}/share/qemu/edk2-arm-vars.fd"

          # Check sqlx query macros against the committed `.sqlx` cache, matching
          # CI (nix/lib.nix). Without this the `database` shell exports a
          # DATABASE_URL for its empty ephemeral Postgres, so the macros try to
          # compile against an unmigrated DB and fail. `cargo sqlx prepare`
          # overrides this back to false internally, so the cache can still be
          # regenerated from here.
          export SQLX_OFFLINE="true"

          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:''${PKG_CONFIG_PATH:-}"
          export OPENSSL_DIR="${pkgs.openssl.dev}"
          export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
          export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
          export LD_LIBRARY_PATH="${pkgs.openssl.out}/lib:''${LD_LIBRARY_PATH:-}"
        '';
      };

      databaseShell = pkgs.mkShell {
        name = "treadmill-db-migrate-shell";

        # `mkShell { packages = [ ...]; }` gets turned into `nativeBuildInputs`:
        packages =
          defaultShell.nativeBuildInputs
          ++ (with pkgs; [
            atlas
            sqlx-cli
          ]);

        shellHook = defaultShell.shellHook + ''
          export PG_BASE_DIR="$(mktemp -d /tmp/tmlswbmigratedb-XXXXX)"

          echo "Initializing new Postgres database in $PG_BASE_DIR"
          initdb -D "$PG_BASE_DIR"

          pg_ctl -D "$PG_BASE_DIR" -l "$PG_BASE_DIR/log" \
            -o "-h ''' --unix_socket_directories='$PG_BASE_DIR'" start

          # Tear down Postgres and its scratch dir once the shell is gone. A
          # plain `trap ... EXIT` is not enough: `nix develop -c CMD` execs the
          # command, replacing the shell that holds the trap, so it never fires
          # and the detached pg_ctl daemon is orphaned in /tmp. Instead, fork a
          # watcher keyed on this shell's PID. exec preserves the PID, so the
          # watcher fires on every exit path -- interactive exit, `-c`, even a
          # killed terminal -- and only ever touches its own cluster, so
          # concurrent database shells don't clean up each other.
          __tml_db_pid=$$
          (
            while kill -0 "$__tml_db_pid" 2>/dev/null; do sleep 1; done
            pg_ctl -D "$PG_BASE_DIR" stop -m immediate >/dev/null 2>&1
            rm -rf "$PG_BASE_DIR"
          ) &
          disown

          export PGHOST="$PG_BASE_DIR"
          export PGHOST_URIENCODE="$(printf '%s' "$PGHOST" | ${pkgs.jq}/bin/jq -sRr @uri)"
          export PGUSER="$(whoami)"
          export PGDATABASE="postgres"
          export DATABASE_URL="postgresql://''${PGUSER}@''${PGHOST_URIENCODE}/''${PGDATABASE}"

          export TML_DATABASE__HOST="$PGHOST"
          export TML_DATABASE__DATABASE="$PGDATABASE"
          export TML_DATABASE__USER="$PGUSER"
        '';
      };

      # Image-build shell: the libguestfs pipeline tooling
      # (doc/images-libguestfs-build-plan.md). Standalone — does NOT inherit the
      # default shell's Rust/Postgres/NATS stack, since `images/lib/build-image.sh`
      # is plain shell and consumes the `tml-puppet` / `image-util` binaries built
      # separately via `nix build`. Image builds need privileged libguestfs and
      # network (live apt), so they run outside the Nix sandbox in this shell, not
      # as a hermetic check.
      imagesShell = pkgs.mkShell {
        name = "treadmill-images-shell";
        packages = with pkgs; [
          # guestfish + the C virt-* tools (virt-copy-in, virt-filesystems, …),
          # with the prebuilt appliance bundled (no supermin build on first use).
          libguestfs-with-appliance
          # virt-customize/virt-sysprep live in the separate guestfs-tools
          # package (the OCaml tools were split out of libguestfs upstream); it
          # reuses the appliance above.
          guestfs-tools
          # mcopy/mtype/mdir: edit cmdline.txt / ssh.txt in the raw FAT boot blob
          # (boot layers have no overlay mechanism, so the FAT is mutated in place).
          mtools
          # qemu-img: raw<->qcow2 conversion and the overlay backing-chain dance.
          qemu-utils
          # decompress .img.xz base images (Raspberry Pi OS lite).
          xz
          # fetch base images with mirror fallback.
          curl
          coreutils
        ];

        shellHook = ''
          # libguestfs boots its appliance under qemu; the direct backend uses
          # /dev/kvm when present (matching-arch, no TCG) and is the simplest
          # backend for CI/sandbox use.
          export LIBGUESTFS_BACKEND=direct
        '';
      };
    in
    {
      devShells = {
        default = defaultShell;
        database = databaseShell;
        images = imagesShell;
      };
    };
}
