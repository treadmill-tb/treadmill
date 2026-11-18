//! Producing Treadmill disk images.
//!
//! [`treadmill_rs::image`] defines the image *format* — what a manifest says,
//! and what makes one valid. This crate is the producer side of it: turning
//! blobs on disk into a layout a registry will accept, and checking that what
//! came out is what was meant.
//!
//! | | |
//! |---|---|
//! | [`layout`] | an OCI layout directory: its blobs, its index, its manifest |
//! | [`assemble`], [`append`], [`verify`] | writing a layout, and checking one |
//! | [`registry`] | moving a layout to and from an OCI registry |
//! | [`layer_arg`] | the `ROLE=FORMAT:PATH` and `ROLE=LAYERS` grammars |
//!
//! Nothing here parses a command line or writes to a terminal. `image-util`
//! is the thin CLI over it.

pub mod append;
pub mod assemble;
pub mod layer_arg;
pub mod layout;
pub mod registry;
pub mod verify;
