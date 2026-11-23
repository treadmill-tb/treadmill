//! Building Treadmill disk images.
//!
//! [`treadmill_rs::image`] defines what a Treadmill image *is* as a format.
//! This crate is what produces one, and `tml image` is the front-end over it.
//!
//! An image is a directory: an `image.json`, a `provision.sh`, and whatever
//! that provisioning needs. What it is built on is its `base` — another
//! directory, an image in a registry, or nothing at all — so a chain is a
//! path through the file system and works the same wherever the directories
//! live. See [`tree`].
//!
//! | | |
//! |---|---|
//! | [`tree`] | following `base` from one image to the next |
//! | [`builder`] | building one image, and the chain beneath it |
//! | [`spec`] | what a resolved directory says about itself |
//! | [`layout`] | an OCI layout directory: its blobs, its index, its manifest |
//! | [`assemble`], [`append`], [`verify`] | writing a layout, and checking one |
//! | [`registry`] | moving a layout to and from an OCI registry |
//! | [`qemu`], [`nbd`], [`mount`], [`nspawn`] | the machinery a build is made of |
//! | [`cleanup`] | undoing it, including when interrupted |
//! | [`exec`] | running the tools, and escalating the ones that need root |
//! | [`layer_arg`] | the `ROLE=FORMAT:PATH` and `ROLE=LAYERS` grammars |
//!
//! Nothing here parses a command line or writes to a terminal. Building needs
//! a privileged Linux host; see the crate README for what has to be on `PATH`
//! and for the handful of invariants that look arbitrary and are not.

pub mod append;
pub mod assemble;
pub mod builder;
pub mod cleanup;
pub mod exec;
pub mod layer_arg;
pub mod layout;
pub mod mount;
pub mod nbd;
pub mod nspawn;
pub mod qemu;
pub mod registry;
pub mod spec;
pub mod tree;
pub mod verify;
