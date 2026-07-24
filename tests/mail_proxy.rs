mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use base64::Engine as _;
use support::sluice_env::{NGINX_AUTH_SMTP_ADDR, NGINX_UNAUTH_SMTP_ADDR, ROUTE_A_DOMAIN, ROUTE_B_DOMAIN, get_env};

struct SmtpClient {
	stream: TcpStream,
	reader: BufReader<TcpStream>,
}

impl SmtpClient {
	fn connect(addr: SocketAddr) -> Self {
		let stream = TcpStream::connect(addr).expect("failed to connect to nginx");
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

fn base64_plain_auth(user: &str, pass: &str) -> String {
	base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"))
}
