mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::time::Duration;

use base64::Engine as _;
use support::sluice_env::{
	DUMMY_UPSTREAM_A_ADDR, DUMMY_UPSTREAM_B_ADDR, NGINX_AUTH_SMTP_ADDR, NGINX_IMAP_ADDR, NGINX_POP3_ADDR,
	NGINX_UNAUTH_SMTP_ADDR, ROUTE_A_DOMAIN, ROUTE_AUTH_ONLY_DOMAIN, ROUTE_B_DOMAIN, ROUTE_IMAP_ONLY_DOMAIN, ROUTE_IP_A,
	ROUTE_IP_AUTH_ONLY, ROUTE_IP_B, ROUTE_IP_RELAY_CLIENT, ROUTE_SMTP_ONLY_DOMAIN, ROUTE_SPLIT_DOMAIN, get_env,
};

struct SmtpClient {
	stream: TcpStream,
	reader: BufReader<TcpStream>,
}

impl SmtpClient {
	fn connect(addr: SocketAddr) -> Self {
		Self::from_stream(TcpStream::connect(addr).expect("failed to connect to nginx"))
	}

	/// Connects with the client socket bound to `source`, so nginx reports it as `Client-IP`. Anything in
	/// 127.0.0.0/8 works on Linux without extra setup.
	fn connect_from(addr: SocketAddr, source: &str) -> Self {
		let source: IpAddr = source.parse().expect("source is a valid IP");
		let (IpAddr::V4(source), SocketAddr::V4(addr)) = (source, addr) else {
			panic!("connect_from only supports IPv4");
		};
		let stream = unsafe {
			let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
			assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
			// Owning the fd from here on closes it on any panic below.
			let owned = OwnedFd::from_raw_fd(fd);
			let sockaddr = |ip: std::net::Ipv4Addr, port: u16| libc::sockaddr_in {
				sin_family: libc::AF_INET as libc::sa_family_t,
				sin_port: port.to_be(),
				sin_addr: libc::in_addr {
					s_addr: u32::from(ip).to_be(),
				},
				sin_zero: [0; 8],
			};
			let size = size_of::<libc::sockaddr_in>() as libc::socklen_t;
			let local = sockaddr(source, 0);
			let remote = sockaddr(*addr.ip(), addr.port());
			let rc = libc::bind(fd, (&raw const local).cast(), size);
			assert_eq!(rc, 0, "bind() failed: {}", std::io::Error::last_os_error());
			let rc = libc::connect(fd, (&raw const remote).cast(), size);
			assert_eq!(rc, 0, "connect() failed: {}", std::io::Error::last_os_error());
			TcpStream::from(owned)
		};
		Self::from_stream(stream)
	}

	fn from_stream(stream: TcpStream) -> Self {
		let reader = BufReader::new(stream.try_clone().expect("failed to clone stream"));
		let mut client = Self { stream, reader };
		client.read_response(); // greeting
		client
	}

	fn send(&mut self, line: &str) {
		self.stream
			.write_all(line.as_bytes())
			.expect("failed to write to nginx");
		self.stream.write_all(b"\r\n").expect("failed to write to nginx");
	}

	/// Reads a full SMTP reply, following continuation lines ("250-...") until the final
	/// line ("250 ...", space instead of dash).
	fn read_response(&mut self) -> String {
		let mut full = String::new();
		loop {
			let mut line = String::new();
			self.reader.read_line(&mut line).expect("failed to read from nginx");
			let is_final = line.as_bytes().get(3) != Some(&b'-');
			full.push_str(&line);
			if is_final {
				return full;
			}
		}
	}

	fn command(&mut self, line: &str) -> String {
		self.send(line);
		self.read_response()
	}
}

/// Unauthenticated connections route on the RCPT TO domain (the primary lookup in the
/// `Auth-Method: none` branch of `auth()`); MAIL FROM here is deliberately in an unrelated
/// domain to prove routing didn't just fall back to it.
#[test]
fn routes_unauthenticated_connection_by_recipient_domain() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_A_DOMAIN}>"));
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-a");
	assert!(
		hit.lines
			.iter()
			.any(|l| l.contains("MAIL FROM") && l.contains("unrelated.test"))
	);
	assert!(
		hit.lines
			.iter()
			.any(|l| l.contains("RCPT TO") && l.contains(ROUTE_A_DOMAIN))
	);
}

/// Authenticated connections route on the AUTH username's domain (nginx queries
/// `auth_http` right after AUTH completes, before MAIL FROM/RCPT TO exist yet - see
/// `auth()` in src/http.rs). MAIL FROM/RCPT TO here are in an unrelated domain to prove
/// routing didn't use them.
#[test]
fn routes_authenticated_connection_by_auth_user_domain() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth(&format!("user@{ROUTE_B_DOMAIN}"), "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(auth_resp.starts_with("235"), "unexpected AUTH response: {auth_resp}");

	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	assert!(client.command("RCPT TO:<rcpt@unrelated.test>").starts_with("250"));
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-b");
}

/// When neither the RCPT TO nor the MAIL FROM domain is managed, `auth()` falls all the
/// way through to `ProxyTableLookupError::lookup_failed()` (550 5.7.1) and nginx must
/// never connect to either upstream.
#[test]
fn unauthenticated_rejects_when_neither_domain_is_managed() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unmanaged-a.test>").starts_with("250"));
	let rcpt_resp = client.command("RCPT TO:<user@unmanaged-b.test>");
	assert!(
		rcpt_resp.starts_with("550 5.7.1"),
		"unexpected RCPT TO response: {rcpt_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// Same lookup-failure case, but authenticated: nginx queries `auth_http` right at AUTH
/// time, so an unmanaged `Auth-User` domain must fail the AUTH command itself (before
/// MAIL FROM/RCPT TO are ever sent), with the same 550 5.7.1 `auth_lookup_failed()` code.
#[test]
fn authenticated_rejects_when_domain_is_not_managed() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth("user@unmanaged.test", "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(
		auth_resp.starts_with("550 5.7.1"),
		"unexpected AUTH response: {auth_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// A syntactically-invalid RCPT TO address fails `EmailAddress::from_str` and is reported
/// as `InvalidRcpt` (501 5.1.3), independent of whether any domain is managed.
#[test]
fn unauthenticated_rejects_invalid_recipient_address() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	let rcpt_resp = client.command("RCPT TO:<not-an-email>");
	assert!(
		rcpt_resp.starts_with("501 5.1.3"),
		"unexpected RCPT TO response: {rcpt_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// When the RCPT TO domain isn't managed, `auth()` falls back to parsing MAIL FROM; if
/// *that* isn't a valid address either, it's reported as `InvalidFrom` (501 5.1.7).
#[test]
fn unauthenticated_rejects_invalid_sender_address_on_fallback() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<not-an-email>").starts_with("250"));
	let rcpt_resp = client.command("RCPT TO:<user@unmanaged.test>");
	assert!(
		rcpt_resp.starts_with("501 5.1.7"),
		"unexpected RCPT TO response: {rcpt_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// An authenticated connection whose `Auth-User` isn't a valid e-mail address (`auth()`
/// assumes it is, since that's all it has to route on) is reported as `InvalidFrom`
/// (501 5.1.7) at AUTH time.
#[test]
fn authenticated_rejects_invalid_auth_user() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth("not-an-email", "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(
		auth_resp.starts_with("501 5.1.7"),
		"unexpected AUTH response: {auth_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// When MAIL FROM and RCPT TO are in two different *managed* domains, RCPT TO must win -
/// this is what distinguishes genuine RCPT-first precedence from merely falling back to
/// MAIL FROM because RCPT TO didn't resolve (as in the other routing test).
#[test]
fn unauthenticated_prefers_recipient_domain_when_both_are_managed() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(
		client
			.command(&format!("MAIL FROM:<sender@{ROUTE_B_DOMAIN}>"))
			.starts_with("250")
	);
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_A_DOMAIN}>"));
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-a");
}

/// When the RCPT TO domain isn't managed but the MAIL FROM domain *is*, `auth()` must
/// actually fall back to routing on it - this is the mirror of
/// `unauthenticated_rejects_invalid_sender_address_on_fallback`, which only exercises the
/// fallback's *failure* arm.
#[test]
fn unauthenticated_falls_back_to_sender_domain_when_recipient_is_unmanaged() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(
		client
			.command(&format!("MAIL FROM:<sender@{ROUTE_B_DOMAIN}>"))
			.starts_with("250")
	);
	let rcpt_resp = client.command("RCPT TO:<user@unmanaged.test>");
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-b");
}

/// A domain mapped with `NetSocketAddrs::ByProtocol` routes an unauthenticated SMTP session
/// on its `smtp` entry. Paired with `routes_authenticated_session_on_the_smtp_authenticated_entry`:
/// the same domain, differing only in whether the client authenticated, must land on two
/// different upstreams - that split is the whole point of the variant (port 25 vs 587
/// upstream, since nginx never tells sluice which port the client connected to).
#[test]
fn routes_unauthenticated_session_on_the_smtp_entry() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_SPLIT_DOMAIN}>"));
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-a");
}

/// The other half of the `ByProtocol` split: authenticating promotes `Smtp` to
/// `SmtpAuthenticated` (`MailProtocol::set_authenticated`), so the identical domain must now
/// resolve to the *other* upstream.
#[test]
fn routes_authenticated_session_on_the_smtp_authenticated_entry() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth(&format!("user@{ROUTE_SPLIT_DOMAIN}"), "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(auth_resp.starts_with("235"), "unexpected AUTH response: {auth_resp}");

	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	assert!(client.command("RCPT TO:<rcpt@unrelated.test>").starts_with("250"));
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-b");
}

/// A `ByProtocol` domain with no entry for the protocol in play is a *managed* domain that
/// still can't be routed - `ProtocolLookupFailed`. Since the `smtp_authenticated` -> `smtp`
/// fallback absorbs the "operator forgot a key" case, what's left here is deliberate policy,
/// so it's a permanent `550 5.7.0` - distinguishable in logs from the `550 5.7.1` an entirely
/// unmanaged domain gets, but the same class of refusal.
///
/// The full code is asserted, not just the enhanced part, because the basic code is derived
/// from `basic_detail` rather than given directly (see `SmtpStatus`'s `Display`) - it's the
/// half that's easy to get wrong without noticing.
#[test]
fn unauthenticated_rejects_when_domain_has_no_entry_for_this_protocol() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_IMAP_ONLY_DOMAIN}>"));
	assert!(
		rcpt_resp.starts_with("550 5.7.0"),
		"unexpected RCPT TO response: {rcpt_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// Same protocol-missing case at AUTH time. This is the arm most likely to bite an operator:
/// a domain mapped with an `smtp` entry but no `smtp_authenticated` one rejects every
/// submission, because authenticating changes which key is looked up.
#[test]
fn authenticated_rejects_when_domain_has_no_entry_for_this_protocol() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth(&format!("user@{ROUTE_IMAP_ONLY_DOMAIN}"), "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(
		auth_resp.starts_with("550 5.7.0"),
		"unexpected AUTH response: {auth_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// A `ByProtocol` domain with an `smtp` entry but no `smtp_authenticated` one falls back to
/// the `smtp` entry rather than failing, so the common "same server, no port split" config
/// doesn't have to name both keys.
///
/// The fallback is deliberately one-way - see
/// `unauthenticated_does_not_fall_back_to_the_smtp_authenticated_entry` for the direction
/// that must *not* work.
#[test]
fn authenticated_falls_back_to_the_smtp_entry() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth(&format!("user@{ROUTE_SMTP_ONLY_DOMAIN}"), "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(auth_resp.starts_with("235"), "unexpected AUTH response: {auth_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-a");
}

/// The fallback must not run in reverse: a domain mapped *only* for `smtp_authenticated` must
/// reject unauthenticated SMTP rather than routing it to the authenticated entry. That entry
/// is the submission port (587) in the setup this variant exists for, and quietly delivering
/// unauthenticated inbound mail to a submission endpoint is exactly the thing an operator
/// splitting the ports is trying to prevent.
#[test]
fn unauthenticated_does_not_fall_back_to_the_smtp_authenticated_entry() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_UNAUTH_SMTP_ADDR);

	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unrelated.test>").starts_with("250"));
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_AUTH_ONLY_DOMAIN}>"));
	assert!(
		rcpt_resp.starts_with("550 5.7.0"),
		"unexpected RCPT TO response: {rcpt_resp}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// A `Single` mapping is protocol-agnostic by construction, so it must answer for an
/// authenticated session too - i.e. adding `MailProtocol` didn't quietly break the plain
/// `"domain" = "host:port"` config form that every pre-existing config uses.
#[test]
fn single_address_mapping_answers_for_every_protocol() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect(NGINX_AUTH_SMTP_ADDR);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth(&format!("user@{ROUTE_A_DOMAIN}"), "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(auth_resp.starts_with("235"), "unexpected AUTH response: {auth_resp}");
	drop(client);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-a");
}

/// nginx only ever sends `Auth-Protocol` values it supports, so these two arms of `auth()`
/// are unreachable through a real SMTP session and are exercised against sluice's HTTP
/// endpoint directly. Both must fail closed (`ProtocolUnknown`) rather than defaulting to
/// some protocol and routing on it.
///
/// `451 4.3.5` and not a 5xx: reaching this means nginx and sluice disagree about protocol
/// names, which is a deployment fault rather than anything the sender did. A *temporary*
/// failure leaves the message queued at the sender to be redelivered once an operator fixes
/// the config, where a permanent one would bounce mail over a server-side mistake - so the
/// 4xx class here is the point of the test, not an incidental detail of the code.
#[test]
fn rejects_unknown_or_missing_auth_protocol() {
	let env = get_env();

	for headers in [
		&[("Auth-Protocol", "gopher"), ("Auth-Method", "none")][..],
		&[("Auth-Method", "none")][..],
	] {
		let response = env.raw_auth_request(headers);
		assert_eq!(
			response.get("auth-error-code").map(String::as_str),
			Some("451 4.3.5"),
			"unexpected response to {headers:?}: {response:?}"
		);
		assert_ne!(
			response.get("auth-status").map(String::as_str),
			Some("OK"),
			"sluice routed a request it couldn't identify the protocol of: {response:?}"
		);
		assert!(
			!response.contains_key("auth-server"),
			"a rejected request must not carry an upstream: {response:?}"
		);
	}
}

/// Where a hand-built `/auth` request was routed, as the `Auth-Server`/`Auth-Port` pair nginx would connect to, or
/// `None` if it was rejected.
fn routed_to(response: &std::collections::HashMap<String, String>) -> Option<SocketAddr> {
	if response.get("auth-status").map(String::as_str) != Some("OK") {
		assert!(
			!response.contains_key("auth-server"),
			"a rejected request must not carry an upstream: {response:?}"
		);
		return None;
	}
	Some(
		format!("{}:{}", response["auth-server"], response["auth-port"])
			.parse()
			.expect("sluice sent an unparseable upstream"),
	)
}

fn unauthenticated_request<'a>(client_ip: Option<&'a str>, rcpt: &'a str, from: &'a str) -> Vec<(&'a str, &'a str)> {
	let mut headers = vec![
		("Auth-Protocol", "smtp"),
		("Auth-Method", "none"),
		("Auth-SMTP-To", rcpt),
		("Auth-SMTP-From", from),
	];
	headers.extend(client_ip.map(|ip| ("Client-IP", ip)));
	headers
}

fn authenticated_request<'a>(client_ip: Option<&'a str>, user: &'a str) -> Vec<(&'a str, &'a str)> {
	let mut headers = vec![("Auth-Protocol", "smtp"), ("Auth-Method", "plain"), ("Auth-User", user)];
	headers.extend(client_ip.map(|ip| ("Client-IP", ip)));
	headers
}

/// The relay use case: an unmanaged recipient from a client with an IP rule goes to that rule's upstream.
#[test]
fn unauthenticated_falls_back_to_client_ip_when_recipient_is_unmanaged() {
	let env = get_env();
	let response = env.raw_auth_request(&unauthenticated_request(
		Some(ROUTE_IP_A),
		"RCPT TO:<someone@example.org>",
		"MAIL FROM:<sender@unmanaged.test>",
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_A_ADDR), "{response:?}");

	let response = env.raw_auth_request(&unauthenticated_request(
		Some(ROUTE_IP_B),
		"RCPT TO:<someone@example.org>",
		"MAIL FROM:<sender@unmanaged.test>",
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_B_ADDR), "{response:?}");
}

/// A managed recipient domain beats the client IP: the relay client's mail for local domains still goes to the
/// local server, only everything else goes to the relay.
#[test]
fn unauthenticated_recipient_domain_beats_client_ip() {
	let env = get_env();
	let response = env.raw_auth_request(&unauthenticated_request(
		Some(ROUTE_IP_A),
		&format!("RCPT TO:<user@{ROUTE_B_DOMAIN}>"),
		"MAIL FROM:<sender@unmanaged.test>",
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_B_ADDR), "{response:?}");
}

/// Client IP is checked before the MAIL FROM domain fallback (deliberate; see the relay use case). Pinned so that
/// reordering them is a conscious decision.
#[test]
fn unauthenticated_client_ip_beats_sender_domain() {
	let env = get_env();
	let response = env.raw_auth_request(&unauthenticated_request(
		Some(ROUTE_IP_A),
		"RCPT TO:<someone@example.org>",
		&format!("MAIL FROM:<sender@{ROUTE_B_DOMAIN}>"),
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_A_ADDR), "{response:?}");
}

/// An IP with no rule changes nothing: the sender-domain fallback still works, and with nothing matching the
/// request is rejected the usual way.
#[test]
fn unauthenticated_client_ip_without_rule_changes_nothing() {
	let env = get_env();
	let response = env.raw_auth_request(&unauthenticated_request(
		Some("192.0.2.99"),
		"RCPT TO:<someone@example.org>",
		&format!("MAIL FROM:<sender@{ROUTE_B_DOMAIN}>"),
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_B_ADDR), "{response:?}");

	let response = env.raw_auth_request(&unauthenticated_request(
		Some("192.0.2.99"),
		"RCPT TO:<someone@example.org>",
		"MAIL FROM:<sender@unmanaged.test>",
	));
	assert_eq!(routed_to(&response), None, "{response:?}");
	assert_eq!(response.get("auth-error-code").map(String::as_str), Some("550 5.7.1"));
}

/// Authenticated sessions use the user's domain first and only then the client IP.
#[test]
fn authenticated_routes_by_user_domain_then_client_ip() {
	let env = get_env();
	let response = env.raw_auth_request(&authenticated_request(
		Some(ROUTE_IP_A),
		&format!("user@{ROUTE_B_DOMAIN}"),
	));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_B_ADDR), "{response:?}");

	let response = env.raw_auth_request(&authenticated_request(Some(ROUTE_IP_A), "user@unmanaged.test"));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_A_ADDR), "{response:?}");

	let response = env.raw_auth_request(&authenticated_request(Some("192.0.2.99"), "user@unmanaged.test"));
	assert_eq!(routed_to(&response), None, "{response:?}");
	assert_eq!(response.get("auth-error-code").map(String::as_str), Some("550 5.7.1"));
}

/// Client IPs compare by canonical address, not by spelling: nginx prints a dual-stack listener's IPv4 clients as
/// `::ffff:a.b.c.d`, and IPv6 can be written many ways.
#[test]
fn client_ip_matches_regardless_of_spelling() {
	let env = get_env();
	for (client_ip, expected) in [
		("::ffff:192.0.2.1", DUMMY_UPSTREAM_A_ADDR),
		("::FFFF:192.0.2.1", DUMMY_UPSTREAM_A_ADDR),
		("2001:db8:0:0:0:0:0:1", DUMMY_UPSTREAM_B_ADDR),
		("2001:0DB8::1", DUMMY_UPSTREAM_B_ADDR),
	] {
		let response = env.raw_auth_request(&unauthenticated_request(
			Some(client_ip),
			"RCPT TO:<someone@example.org>",
			"MAIL FROM:<sender@unmanaged.test>",
		));
		assert_eq!(routed_to(&response), Some(expected), "{client_ip}: {response:?}");
	}
}

/// The matching IP rule has no entry for the protocol in play, so it is a managed client with nowhere to go.
#[test]
fn client_ip_rule_without_entry_for_protocol_is_rejected() {
	let env = get_env();
	let response = env.raw_auth_request(&unauthenticated_request(
		Some(ROUTE_IP_AUTH_ONLY),
		"RCPT TO:<someone@example.org>",
		"MAIL FROM:<sender@unmanaged.test>",
	));
	assert_eq!(routed_to(&response), None, "{response:?}");
	assert_eq!(response.get("auth-error-code").map(String::as_str), Some("550 5.7.0"));

	let response = env.raw_auth_request(&authenticated_request(Some(ROUTE_IP_AUTH_ONLY), "user@unmanaged.test"));
	assert_eq!(routed_to(&response), Some(DUMMY_UPSTREAM_A_ADDR), "{response:?}");
}

/// nginx sends `unix:/path` (or just `unix:`) for unix-socket listeners, and a header could be missing or garbage.
/// None of those can match an IP rule, but must not break domain routing or panic.
#[test]
fn unusable_client_ip_disables_only_ip_matching() {
	let env = get_env();
	for client_ip in [
		None,
		Some("unix:/run/nginx-mail.sock"),
		Some("unix:"),
		Some(""),
		Some("not an ip"),
	] {
		let response = env.raw_auth_request(&unauthenticated_request(
			client_ip,
			&format!("RCPT TO:<user@{ROUTE_A_DOMAIN}>"),
			"MAIL FROM:<sender@unmanaged.test>",
		));
		assert_eq!(
			routed_to(&response),
			Some(DUMMY_UPSTREAM_A_ADDR),
			"{client_ip:?}: {response:?}"
		);

		let response = env.raw_auth_request(&unauthenticated_request(
			client_ip,
			"RCPT TO:<someone@example.org>",
			"MAIL FROM:<sender@unmanaged.test>",
		));
		assert_eq!(routed_to(&response), None, "{client_ip:?}: {response:?}");
		assert_eq!(
			response.get("auth-error-code").map(String::as_str),
			Some("550 5.7.1"),
			"{client_ip:?}: {response:?}"
		);

		let response = env.raw_auth_request(&authenticated_request(client_ip, "user@unmanaged.test"));
		assert_eq!(routed_to(&response), None, "{client_ip:?}: {response:?}");
	}
}

/// An IP written into the *address* never matches an IP rule; only the real `Client-IP` does. Otherwise a sender
/// could pick their own route (or an open relay) with `MAIL FROM:<x@[192.0.2.1]>`.
#[test]
fn ip_in_email_domain_never_matches_an_ip_rule() {
	let env = get_env();
	for domain in [ROUTE_IP_A, "[192.0.2.1]", "[IPv6:2001:db8::1]"] {
		let response = env.raw_auth_request(&unauthenticated_request(
			Some("192.0.2.99"),
			&format!("RCPT TO:<user@{domain}>"),
			"MAIL FROM:<sender@unmanaged.test>",
		));
		assert_eq!(routed_to(&response), None, "rcpt {domain}: {response:?}");

		let response = env.raw_auth_request(&unauthenticated_request(
			Some("192.0.2.99"),
			"RCPT TO:<someone@example.org>",
			&format!("MAIL FROM:<sender@{domain}>"),
		));
		assert_eq!(routed_to(&response), None, "sender {domain}: {response:?}");

		let response = env.raw_auth_request(&authenticated_request(Some("192.0.2.99"), &format!("user@{domain}")));
		assert_eq!(routed_to(&response), None, "login {domain}: {response:?}");
	}
}

/// End to end through real nginx: the relay client's own address becomes `Client-IP`. Domains sluice manages still
/// go to the local upstream (A); everything else goes to the relay (B).
#[test]
fn routes_unauthenticated_session_by_real_client_ip() {
	let env = get_env();
	let _session = env.lock_session();

	let mut client = SmtpClient::connect_from(NGINX_UNAUTH_SMTP_ADDR, ROUTE_IP_RELAY_CLIENT);
	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unmanaged.test>").starts_with("250"));
	let rcpt_resp = client.command("RCPT TO:<someone@example.org>");
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);
	assert_eq!(env.recv_upstream_hit(Duration::from_secs(5)).upstream, "route-b");

	let mut client = SmtpClient::connect_from(NGINX_UNAUTH_SMTP_ADDR, ROUTE_IP_RELAY_CLIENT);
	assert!(client.command("HELO test.local").starts_with("250"));
	assert!(client.command("MAIL FROM:<sender@unmanaged.test>").starts_with("250"));
	let rcpt_resp = client.command(&format!("RCPT TO:<user@{ROUTE_A_DOMAIN}>"));
	assert!(rcpt_resp.starts_with("250"), "unexpected RCPT TO response: {rcpt_resp}");
	drop(client);
	assert_eq!(env.recv_upstream_hit(Duration::from_secs(5)).upstream, "route-a");
}

/// Same, authenticated: nginx asks at AUTH time, so a login from an unmanaged domain is routed by client IP.
#[test]
fn routes_authenticated_session_by_real_client_ip() {
	let env = get_env();
	let _session = env.lock_session();
	let mut client = SmtpClient::connect_from(NGINX_AUTH_SMTP_ADDR, ROUTE_IP_RELAY_CLIENT);

	assert!(client.command("EHLO test.local").contains("AUTH"));
	let creds = base64_plain_auth("user@unmanaged.test", "dummy-password");
	let auth_resp = client.command(&format!("AUTH PLAIN {creds}"));
	assert!(auth_resp.starts_with("235"), "unexpected AUTH response: {auth_resp}");
	drop(client);

	assert_eq!(env.recv_upstream_hit(Duration::from_secs(5)).upstream, "route-b");
}

fn base64_plain_auth(user: &str, pass: &str) -> String {
	base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"))
}

/// Sends one IMAP `LOGIN` and returns the tagged response line.
fn imap_login(addr: SocketAddr, user: &str) -> String {
	let stream = TcpStream::connect(addr).expect("failed to connect to nginx");
	let mut reader = BufReader::new(stream.try_clone().expect("failed to clone stream"));
	let mut line = String::new();
	reader.read_line(&mut line).expect("failed to read IMAP greeting");

	let mut stream = stream;
	stream
		.write_all(format!("a1 LOGIN {user} dummy-password\r\n").as_bytes())
		.expect("failed to write to nginx");
	let mut response = String::new();
	reader.read_line(&mut response).expect("failed to read IMAP response");
	response
}

/// Sends a POP3 `USER`/`PASS` pair and returns the response to `PASS` (which is when nginx
/// queries `auth_http`).
fn pop3_login(addr: SocketAddr, user: &str) -> String {
	let stream = TcpStream::connect(addr).expect("failed to connect to nginx");
	let mut reader = BufReader::new(stream.try_clone().expect("failed to clone stream"));
	let mut line = String::new();
	reader.read_line(&mut line).expect("failed to read POP3 greeting");

	let mut stream = stream;
	for command in [format!("USER {user}\r\n"), "PASS dummy-password\r\n".to_string()] {
		stream.write_all(command.as_bytes()).expect("failed to write to nginx");
		line.clear();
		reader.read_line(&mut line).expect("failed to read POP3 response");
	}
	line
}

/// IMAP sessions route on the `imap` entry. `ROUTE_IMAP_ONLY_DOMAIN` has *only* that entry,
/// so this is the exact mirror of `unauthenticated_rejects_when_domain_has_no_entry_for_this_protocol`,
/// which shows the same domain refusing SMTP.
///
#[test]
fn routes_imap_session_to_the_imap_entry() {
	let env = get_env();
	let _session = env.lock_session();

	let response = imap_login(NGINX_IMAP_ADDR, &format!("user@{ROUTE_IMAP_ONLY_DOMAIN}"));
	assert!(
		!response.contains(" NO ") && !response.contains(" BAD "),
		"IMAP login should have been accepted and proxied, got: {response}"
	);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-imap");
}

/// POP3 sessions route on the `pop3` entry. `ROUTE_SPLIT_DOMAIN` points each of its three
/// protocols at a different upstream, so landing on the POP3 one proves the protocol actually
/// drove the lookup rather than the domain resolving to a single address for everything.
#[test]
fn routes_pop3_session_to_the_pop3_entry() {
	let env = get_env();
	let _session = env.lock_session();

	let response = pop3_login(NGINX_POP3_ADDR, &format!("user@{ROUTE_SPLIT_DOMAIN}"));
	assert!(
		!response.starts_with("-ERR"),
		"POP3 login should have been accepted and proxied, got: {response}"
	);

	let hit = env.recv_upstream_hit(Duration::from_secs(5));
	assert_eq!(hit.upstream, "route-pop3");
}

/// sluice's error messages are SMTP-shaped (they carry an `Auth-Error-Code` like `550 5.7.0`),
/// but nginx discards that header for IMAP and answers with a tagged `NO` instead - so the
/// numeric code never reaches an IMAP client and can't corrupt the response.
///
/// The message *text* is deliberately not asserted here. It's currently phrased for SMTP
/// ("the sending domain..."), which reads oddly for a mailbox login; pinning the exact wording
/// would make this test an obstacle to fixing that rather than a check on protocol framing.
#[test]
fn imap_rejects_unmanaged_domain_without_leaking_an_smtp_code() {
	let env = get_env();
	let _session = env.lock_session();

	let response = imap_login(NGINX_IMAP_ADDR, "user@unmanaged.test");
	assert!(
		response.starts_with("a1 NO "),
		"expected a tagged IMAP NO response, got: {response}"
	);
	assert!(
		!response.contains("550") && !response.contains("5.7.0"),
		"an SMTP status code leaked into an IMAP response: {response}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}

/// The POP3 equivalent: nginx frames the same rejection as `-ERR`, again without the
/// `Auth-Error-Code`.
#[test]
fn pop3_rejects_unmanaged_domain_without_leaking_an_smtp_code() {
	let env = get_env();
	let _session = env.lock_session();

	let response = pop3_login(NGINX_POP3_ADDR, "user@unmanaged.test");
	assert!(
		response.starts_with("-ERR "),
		"expected a POP3 -ERR response, got: {response}"
	);
	assert!(
		!response.contains("550") && !response.contains("5.7.0"),
		"an SMTP status code leaked into a POP3 response: {response}"
	);

	env.assert_no_upstream_hit(Duration::from_millis(200));
}
