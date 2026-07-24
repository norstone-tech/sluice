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
