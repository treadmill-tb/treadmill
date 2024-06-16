//! Treadmill Distributed Testbed Base Crate
//!
//! This crate contains shared type definitions, traits, and helper functions
//! used across other Treadmill components. It does not contain any actual
//! component / service implementations.

pub mod api;
pub mod connector;
pub mod control_socket;
pub mod image;
pub mod supervisor;
pub mod util;
