# Building and Running a Custom Ubuntu VM Image with Nix

## Prerequisites

Ensure you have the following files in your working directory:

- `ubuntu-image.nix`
- `default.nix`
- `run-ubuntu-vm.nix`

Please make sure to replace `YOUR_PUBLIC_SSH_KEY` with your public ssh key

## Steps

1. Build the Ubuntu image:

   ```bash
   nix-build
   ```

2. Copy the image out of the Nix store:

   ```bash
   cp result/disk-image.qcow2 ~/ubuntu-image.qcow2
   ```

3. Set appropriate permissions on the image file:

   ```bash
   chmod 644 ~/ubuntu-image.qcow2
   ```

4. Build the VM runner script:

   ```bash
   nix-build run-ubuntu-vm.nix
   ```

5. Run the VM:

   ```bash
   ./result/bin/run-ubuntu-vm ~/ubuntu-image.qcow2
   ```

6. Once the VM boots, you'll see a login prompt. Log in as root with the password "root".

7. To SSH into the VM from another terminal window:
   ```bash
   ssh -p 2222 root@localhost
   ```
   Use the password "root" when prompted.

## Additional Notes

- To modify the image, edit `ubuntu-image.nix` and repeat steps 1-3.
- To change VM runtime parameters, edit `run-ubuntu-vm.nix` and repeat steps 4-5.
- Remember to copy the image out of the Nix store and reset permissions after each rebuild.
- For security, change the root password after first login.
- Update or remove the SSH key in `ubuntu-image.nix` before sharing the image.
