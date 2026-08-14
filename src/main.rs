use abpl::app::{MainResult, ReloadableService, axum::HotReloadingAxumService, service_main};
use sluice::{
	VERSION_INFO,
	config::{SluiceConfig, SluiceState},
	error::ServiceError,
	http,
};
use tracing::info;

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
		info!("started: {VERSION_INFO}");
		Ok(Self { axum })
	}

	fn reload(&mut self, config: Self::Config) -> Result<(), Self::Error> {
		self.axum.replace_state(SluiceState {
			proxy_map: config.proxy_map,
		});
		self.axum.bind_sockets(config.bind.iter().cloned())?;
		info!("reloaded!");
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
