use abpl::app::{MainResult, ReloadableService, axum::HotReloadingAxumService, service_main};
use const_format::concatcp;

use crate::{
	config::{SluiceConfig, SluiceState},
	error::ServiceError,
};

mod config;
mod error;
mod http;
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

struct SluiceService {
	axum: HotReloadingAxumService<SluiceState>,
}
impl ReloadableService for SluiceService {
	type Error = ServiceError;

	type Config = SluiceConfig;

	fn start(config: Self::Config) -> Result<Self, Self::Error> {
		let mut axum = HotReloadingAxumService::new(
			SluiceState {
				proxy_map: config.proxy_map,
			},
			|_| http::build_router(),
		);
		axum.bind_sockets(config.bind.iter().cloned())?;
		Ok(Self { axum })
	}

	fn reload(&mut self, config: Self::Config) -> Result<(), Self::Error> {
		self.axum.replace_state(SluiceState {
			proxy_map: config.proxy_map,
		});
		self.axum.bind_sockets(config.bind.iter().cloned())?;
		Ok(())
	}

	fn interval(&mut self) -> Result<(), Self::Error> {
		self.axum.restart_dead_threads()?;
		Ok(())
	}

	fn stop(mut self) -> Result<(), Self::Error> {
		self.axum.stop();
		Ok(())
	}
}

fn main() -> MainResult<ServiceError> {
	service_main::<SluiceService>().into()
}
