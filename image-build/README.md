# treadmill-image-build

The producer side of the [Treadmill](https://treadmill.dev) image format.

[`treadmill-rs`](https://crates.io/crates/treadmill-rs) defines what a
Treadmill image *is*: role-tagged chains of qcow2 layers in an OCI manifest,
and the invariants that make one valid. This crate writes one.

| | |
|---|---|
| `layout` | an OCI layout directory: its blobs, its index, its manifest |
| `assemble` | starting an image from layer blobs |
| `append` | adding layers to an image, carrying the lower's through |
| `verify` | checking a built layout against what it claims |
| `registry` | moving a layout to and from an OCI registry |
| `layer_arg` | the `ROLE=FORMAT:PATH` and `ROLE=LAYERS` grammars |

Nothing here parses a command line or writes to a terminal. `image-util` is
the thin CLI over it.

## Requirements

Reading a qcow2 blob's virtual size means asking `qemu-img`, so `qemu-img`
has to be on `PATH`.

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE) or
[MIT license](../LICENSE-MIT) at your option.
