use std::{collections::HashMap, net::SocketAddr as NetSocketAddr};

use abpl::{app::log::ProvidesEnvFilter, types::http::SocketAddr as HttpSocketAddr};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use tracing_subscriber::EnvFilter;

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SluiceConfig {
	pub bind: Vec<HttpSocketAddr>,
	#[serde_as(as = "DisplayFromStr")]
	pub log_filter: EnvFilter,
	pub proxy_map: HashMap<String, NetSocketAddr>,
}
impl ProvidesEnvFilter for SluiceConfig {
	fn log_filter(&self) -> EnvFilter {
		self.log_filter.clone()
	}
}

#[derive(Debug)]
pub struct SluiceState {
	pub proxy_map: HashMap<String, NetSocketAddr>,
}
