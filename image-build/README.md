# treadmill-image-build

Building [Treadmill](https://treadmill.dev) disk images.

[`treadmill-rs`](https://crates.io/crates/treadmill-rs) defines what a
Treadmill image *is* as a format: role-tagged chains of qcow2 layers in an OCI
manifest, and the invariants that make one valid. This crate is what produces
one, and `tml image` is the front-end over it.

## The model

An image is a **directory**: an `image.json`, a `provision.sh`, and whatever
that provisioning needs.

What it is built on is its `base`, which is one of exactly three things, told
apart by how it is written:

- `./x` or `../x` — another image directory, relative to this one.
- empty or absent — nothing. The image fetches its own vendor image.
- anything else — a reference to an image in a registry.

That is the whole model. There is no repository, no coordinate and no required
directory layout, so a chain is a path through the file system and resolves
the same whether the directories sit in one tree or in two different
checkouts — an image kept in your own project can be built on a base kept
somewhere else entirely.

A path has to be written as one. `webide` is a registry reference even if a
directory of that name happens to sit next door, because guessing from what is
on disk would make the meaning of a definition depend on its surroundings.

| | |
|---|---|
| `tree` | following `base` from one image to the next |
| `builder` | building one image, and the chain beneath it |
| `spec` | what an image directory says about itself |
| `layout` | an OCI layout directory: its blobs, its index, its manifest |
| `assemble`, `append`, `verify` | writing a layout, and checking one |
| `registry` | moving a layout to and from an OCI registry |
| `qemu`, `nbd`, `mount`, `nspawn` | the machinery a build is made of |
| `cleanup` | undoing it, including when interrupted |
| `exec` | running the tools, and escalating the ones that need root |

Nothing here parses a command line or writes to a terminal.

## Requirements

Building an image needs a privileged Linux host. `qemu-img`, `qemu-nbd`,
`sfdisk`, `partx`, `blkid`, `mount`, `umount`, `findmnt`, `fstrim`, `growpart`,
`resize2fs`, `e2fsck` and `systemd-nspawn` all have to be on `PATH`; a build
names any that are missing before it starts. Building for a foreign
architecture additionally needs a registered `binfmt_misc` handler, because
provisioning runs the guest's own binaries.

Privilege is per operation rather than per process: the build runs as the user
and escalates each privileged command through `sudo`. One consequence is worth
knowing — the process itself is unprivileged, so nothing here can call
`mount(2)` or its siblings directly, and every privileged step is a subprocess.

## Things that look arbitrary and are not

A shipped blob's backing file is the **empty string**, never a path. Images are
assembled at launch from wherever the supervisor put the blobs, so a baked-in
path would be wrong everywhere but the machine that built it. Blobs therefore
come out of `qemu::finalize_delta` unbaked and have to be relinked
(`qemu::link_chain`) before anything can be stacked on them.

A volume that did not change gets **no layer at all**. Its delta would be
empty, and every empty delta is byte-identical to every other, so unrelated
images would start sharing a digest.

Every device lookup is **scoped to the image's own partitions**. A build host
is often itself a cloud image carrying the same file system labels as the image
being built, and a global `/dev/disk/by-label` lookup can resolve to the host's
own disk.

The host is given back — mounts down, devices detached — **before any layer is
finalized**. A delta read from a live file system is read from a moving target.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or
[MIT license](../LICENSE-MIT) at your option.
