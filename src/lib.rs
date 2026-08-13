//! # sluice is a service, not a library.
//!
//! This library target exists because Rust's integration tests (`tests/`) link against a crate's *library*, so a
//! binary-only crate would have nothing for them to `use`. Everything here is "public" to let those tests reach it.
//! This is not an API anyone is meant to depend on, and it gets published to crates.io only as a side effect of
//! publishing the `sluice` binary.
//!
//! Items may change type, gain enum variants, or disappear entirely; even in a patch release. Version numbers track
//! what service operators and sysadmins see, such as:
//! - The config format
//! - The `/auth` responses
//! - The NixOS module
//!
//! Basically, the guarantee is that blindly installing the next semver compatible version will always work with
//! configs written for previous semver-compatible versions. Anything beyond that, you're on your own.

use const_format::concatcp;

pub mod config;
pub mod error;
pub mod http;
mod smtp_status;

pub const VERSION_INFO: &str = concatcp!(
	env!("CARGO_PKG_NAME"),
	" ",
	env!("BUILD_VERSION"),
	"; rustc ",
	env!("RUSTC_VERSION"),
	"; build-date-time ",
	env!("BUILD_DATETIME"),
	"; build-feature ",
	env!("BUILD_FEATURE"),
	"; build-profile ",
	env!("BUILD_PROFILE"),
	"; build-target ",
	env!("BUILD_TARGET"),
	"; build-target-feature ",
	env!("BUILD_TARGET_FEATURE"),
);
