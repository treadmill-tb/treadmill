with import <nixpkgs> {}; let
  ovmf = pkgs.OVMF.fd;
in
  stdenv.mkDerivation {
    name = "run-ubuntu-vm";
    buildInputs = [pkgs.qemu];

    unpackPhase = "true";

    installPhase = ''
      mkdir -p $out/bin $out/share
      cp ${ovmf}/FV/OVMF_VARS.fd $out/share/OVMF_VARS.fd
      cat > $out/bin/run-ubuntu-vm <<EOF
      #!/bin/sh
      IMAGE_PATH=\''${1:-\$HOME/ubuntu-image.qcow2}
      VARS_FILE=\''${XDG_DATA_HOME:-\$HOME/.local/share}/qemu/OVMF_VARS.fd
      mkdir -p \$(dirname \$VARS_FILE)
      if [ ! -f \$VARS_FILE ]; then
        cp $out/share/OVMF_VARS.fd \$VARS_FILE
      fi
      exec ${pkgs.qemu}/bin/qemu-system-x86_64 \\
        -m 2G \\
        -smp 2 \\
        -drive if=pflash,format=raw,readonly=on,file=${ovmf}/FV/OVMF_CODE.fd \\
        -drive if=pflash,format=raw,file="\$VARS_FILE" \\
        -drive file="\$IMAGE_PATH",if=virtio \\
        -net nic,model=virtio \\
        -net user,hostfwd=tcp::2222-:22 \\
        -nographic
      EOF
      chmod +x $out/bin/run-ubuntu-vm
    '';
  }
