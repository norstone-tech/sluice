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
	config::{MailProtocol, SluiceState},
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
	let authenticated_connection = headers
		.get("Auth-Method")
		.is_some_and(|auth_method| auth_method != "none");

	let mut mail_protocol = headers
		.get("Auth-Protocol")
		.and_then(|header_value| header_value.to_str().ok())
		.map(|header_value| MailProtocol::from_str(header_value).map_err(|_| ProxyTableLookupError::protocol_unknown()))
		.ok_or(ProxyTableLookupError::protocol_unknown())??;
	mail_protocol.set_authenticated(authenticated_connection);

	if authenticated_connection {
		// nginx doesn't send MAIL FROM on authenticated connections, so we have to use the provided user name. Assume
		// that the user name is a valid e-mail address.
		let from = EmailAddress::from_str(
			headers
				.get("Auth-User")
				.and_then(|from| from.to_str().ok())
				.unwrap_or_default(),
		)
		.map_err_invalid_from()?;
		if let Some(upstreams) = state.proxy_map.get(&from.domain().to_ascii_lowercase()) {
			let server = upstreams.get(&mail_protocol)?;
			return Ok(ProxyTableResponse { server });
		};
		Err(ProxyTableLookupError::auth_lookup_failed())
	} else {
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
		if let Some(upstreams) = state.proxy_map.get(&rcpt.domain().to_ascii_lowercase()) {
			let server = upstreams.get(&mail_protocol)?;
			return Ok(ProxyTableResponse { server });
		};
		let from_unchecked = headers
			.get("Auth-SMTP-From")
			.and_then(|from| from.to_str().ok())
			.and_then(|from| regex_captures!(r"^MAIL FROM:\s*<\s*(.*)\s*>\s*", from).map(|(_, from)| from))
			.unwrap_or_default();

		// from may be null, which may be the case in auto-generated bounce messages and the like.
		if !from_unchecked.is_empty()
			&& let Some(upstreams) = state.proxy_map.get(
				&EmailAddress::from_str(from_unchecked)
					.map_err_invalid_from()?
					.domain()
					.to_ascii_lowercase(),
			) {
			// We're going to assume that the upstream server can handle unauthenticated from addresses correctly.
			let server = upstreams.get(&mail_protocol)?;
			return Ok(ProxyTableResponse { server });
		}
		Err(ProxyTableLookupError::lookup_failed())
	}
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
