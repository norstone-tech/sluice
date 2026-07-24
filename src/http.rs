use std::{net::SocketAddr, str::FromStr, sync::Arc};

use abpl::types::hotswap_state::HotswapState;
use axum::{
	extract::State,
	http::HeaderMap,
	response::{AppendHeaders, IntoResponse},
	routing::get,
};
use email_address::EmailAddress;
use lazy_regex::regex_captures;

use crate::{
	config::SluiceState,
	error::{
		ProxyTableLookupError, ResultIntoProxyTableLookupErrorInvalidFrom as _,
		ResultIntoProxyTableLookupErrorInvalidRcpt as _,
	},
	smtp_status::ProvidesSmtpStatus as _,
};

pub fn build_router() -> axum::Router<HotswapState<SluiceState>> {
	axum::Router::new()
		.route("/", get(hello_world))
		.route("/auth", get(auth))
}

async fn hello_world() -> impl IntoResponse {
	super::VERSION_INFO
}

async fn auth(
	State(state): State<Arc<SluiceState>>,
	headers: HeaderMap,
) -> Result<ProxyTableResponse, ProxyTableLookupError> {
	let is_authenticated = headers
		.get("Auth-Method")
		.is_none_or(|auth_method| auth_method == "none");

	// We should only use the sending address for authenticated connections, otherwise sending mail from and to domains
	// managed by the same server would break.
	if is_authenticated {
		// this fails to parse on an empty string, the resulting error is slightly opaque, but only someone who's
		// actively poking around will see it. Still safe!
		let rcpt = EmailAddress::from_str(
			headers
				.get("Auth-SMTP-To")
				.and_then(|from| from.to_str().ok())
				.and_then(|from| regex_captures!(r"^RCPT TO:\s*<\s*(.*)\s*>\s*", from).map(|(_, from)| from))
				.unwrap_or_default(),
		)
		.map_err_invalid_rcpt()?;
		if let Some(upstream) = state.proxy_map.get(&rcpt.domain().to_ascii_lowercase()) {
			return Ok(ProxyTableResponse { server: *upstream });
		};
	}

	let from_unchecked = headers
		.get("Auth-SMTP-From")
		.and_then(|from| from.to_str().ok())
		.and_then(|from| regex_captures!(r"^MAIL FROM:\s*<\s*(.*)\s*>\s*", from).map(|(_, from)| from))
		.unwrap_or_default();

	// from may be null, which may be the case in auto-generated bounce messages and the like.
	if !from_unchecked.is_empty()
		&& let Some(upstream) = state.proxy_map.get(
			&EmailAddress::from_str(from_unchecked)
				.map_err_invalid_from()?
				.domain()
				.to_ascii_lowercase(),
		) {
		return Ok(ProxyTableResponse { server: *upstream });
	}
	Err(if is_authenticated {
		ProxyTableLookupError::auth_lookup_failed()
	} else {
		ProxyTableLookupError::lookup_failed()
	})
}

pub struct ProxyTableResponse {
	pub server: SocketAddr,
}
impl IntoResponse for ProxyTableResponse {
	fn into_response(self) -> axum::response::Response {
		(
			(), // 200 OK with empty body
			AppendHeaders([
				("Auth-Status", "OK".to_string()),
				("Auth-Server", self.server.ip().to_string()),
				("Auth-Port", self.server.port().to_string()),
				// Do I need `Auth-Pass` here?
			]),
		)
			.into_response()
	}
}

impl IntoResponse for ProxyTableLookupError {
	fn into_response(self) -> axum::response::Response {
		(
			(), // 200 OK with empty body
			AppendHeaders([
				("Auth-Error-Code", self.smtp_status().to_string()),
				// Is this replace needed? Do I still have to be 7 bit safe nowadays?
				("Auth-Status", format!("{self:-}").replace('∵', ":")),
				// We can add an Auth-Wait header if we want to keep the connection open, but then we'd have to keep
				// track of connection attempts and I don't want to do that.
			]),
		)
			.into_response()
	}
}
