//! Minimal UEFI boot fixture for the OCI image-migration tests.
//!
//! Prints a single sentinel line to the UEFI console (which QEMU wires to the
//! serial port) and then powers the machine off. The sentinel is chosen at
//! build time via the `TINY_EFI_SENTINEL` environment variable so the same
//! source builds every layer of the two-layer `tiny-efi` chain:
//!
//! - the **base** layer's `BOOTAA64.EFI` prints `BASE-ONLY` (a tripwire: seeing
//!   it means the backing chain was assembled without the overlay), and
//! - the **overlay** layer's `BOOTAA64.EFI` prints `TREADMILL_OK rev=1`.
//!
//! See `doc/oci-image-migration-plan.md` §12.2.
#![no_main]
#![no_std]

use uefi::prelude::*;

/// Sentinel printed to the console. Defaults to the base-layer tripwire so a
/// plain `cargo build` (no Nix, no env) still produces a coherent binary.
const SENTINEL: &str = match option_env!("TINY_EFI_SENTINEL") {
    Some(s) => s,
    None => "BASE-ONLY",
};

#[entry]
fn main() -> Status {
    // Sets up the global allocator + logger and pins the system table the
    // `uefi::println!` macro writes through.
    uefi::helpers::init().unwrap();

    uefi::println!("{SENTINEL}");

    // Give the serial console a moment to flush before the machine vanishes,
    // then power off. `reset` diverges, so control never returns.
    uefi::boot::stall(500_000);
    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None);
}
