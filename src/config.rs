use std::{collections::HashMap, fmt::Display, net::SocketAddr as NetSocketAddr, str::FromStr};

use abpl::{app::log::ProvidesEnvFilter, types::http::SocketAddr as HttpSocketAddr};
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, DisplayFromStr, SerializeDisplay, serde_as};
use tracing_subscriber::EnvFilter;

use crate::error::ProxyTableLookupError;

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum NetSocketAddrs {
	Single(NetSocketAddr),
	// In practice, the key is either "imap", "pop3", or "smtp". But we're just looking up by whatever
	// nginx tells us.
	ByProtocol(HashMap<MailProtocol, NetSocketAddr>),
}
impl NetSocketAddrs {
	pub fn get(&self, mail_protocol: &MailProtocol) -> Result<NetSocketAddr, ProxyTableLookupError> {
		match self {
			NetSocketAddrs::Single(single) => Ok(*single),
			NetSocketAddrs::ByProtocol(by_protocol) => {
				match (by_protocol.get(mail_protocol).copied(), *mail_protocol) {
					(Some(socket_addr), _) => Ok(socket_addr),
					(None, MailProtocol::SmtpAuthenticated) => by_protocol
						.get(&MailProtocol::Smtp)
						.copied()
						.ok_or_else(ProxyTableLookupError::protocol_lookup_failed),
					_ => Err(ProxyTableLookupError::protocol_lookup_failed()),
				}
			},
		}
	}
}

// The default `#[serde(untagged)]` derivation doesn't provide very useful error messages when deserializing.
// Claude suggested this, and I couldn't think of a better solution.
//
// This probably breaks non-human readable formats, even if they're self-described. in fact `NetSocketAddr` explicitly
// expects to _not_ be a string if the format is considered non-human-readable. That said, we're only using toml so we
// gucci.
impl<'de> Deserialize<'de> for NetSocketAddrs {
	fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		struct NetSocketAddrsVisitor;
		impl<'de> serde::de::Visitor<'de> for NetSocketAddrsVisitor {
			type Value = NetSocketAddrs;

			fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				f.write_str("a TCP socket like `\"127.0.0.1:25\"`, or a table like `{smtp = \"127.0.0.1:25\"}`")
			}

			fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
				NetSocketAddr::from_str(v)
					.map(NetSocketAddrs::Single)
					.map_err(E::custom)
			}

			fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
				let mut by_protocol = HashMap::new();
				while let Some((protocol, addr)) = map.next_entry::<MailProtocol, NetSocketAddr>()? {
					by_protocol.insert(protocol, addr);
				}
				Ok(NetSocketAddrs::ByProtocol(by_protocol))
			}
		}
		deserializer.deserialize_any(NetSocketAddrsVisitor)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, SerializeDisplay, DeserializeFromStr, Hash)]
pub enum MailProtocol {
	Smtp,
	// Some mail software behaves differently whether or not port 25 or 587 is used. For example, stalwart by default
	// only does SPF/DKIM/DMARC checks on port 25 and only allows MUAs (i.e., logged in connections) to submit mail on
	// 587. nginx doesn't tell us what port is used when proxying, but what we can do tell nginx to connect to upstream
	// port 587 if someone's trying to log in while speaking SMTP.
	SmtpAuthenticated,
	Imap,
	Pop3,
}
impl MailProtocol {
	pub fn set_authenticated(&mut self, authenticated: bool) {
		match (*self, authenticated) {
			(Self::Smtp, true) => *self = Self::SmtpAuthenticated,
			(Self::SmtpAuthenticated, false) => {
				// Not actually reachable, but hey, completeness
				*self = Self::Smtp
			},
			_ => {
				// sluice is only aware of imap and pop3 connections while they're authenticating
			},
		}
	}
}
impl Display for MailProtocol {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			MailProtocol::Smtp => f.write_str("smtp"),
			MailProtocol::SmtpAuthenticated => f.write_str("smtp_authenticated"),
			MailProtocol::Imap => f.write_str("imap"),
			MailProtocol::Pop3 => f.write_str("pop3"),
		}
	}
}
#[derive(Debug)]
pub struct MailProtocolParseError {}
impl Display for MailProtocolParseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("mail protocol must be \"smtp\", \"smtp_authenticated\", \"imap\", or \"pop3\"")
	}
}
impl std::error::Error for MailProtocolParseError {}

impl FromStr for MailProtocol {
	type Err = MailProtocolParseError;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"smtp" => Ok(Self::Smtp),
			"smtp_authenticated" => Ok(Self::SmtpAuthenticated),
			"imap" => Ok(Self::Imap),
			"pop3" => Ok(Self::Pop3),
			_ => Err(MailProtocolParseError {}),
		}
	}
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SluiceConfig {
	pub bind: Vec<HttpSocketAddr>,
	#[serde_as(as = "DisplayFromStr")]
	pub log_filter: EnvFilter,
	pub proxy_map: HashMap<String, NetSocketAddrs>,
}
impl ProvidesEnvFilter for SluiceConfig {
	fn log_filter(&self) -> EnvFilter {
		self.log_filter.clone()
	}
}

#[derive(Debug)]
pub struct SluiceState {
	pub proxy_map: HashMap<String, NetSocketAddrs>,
}
