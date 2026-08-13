mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use base64::Engine as _;
use support::sluice_env::{
	NGINX_AUTH_SMTP_ADDR, NGINX_IMAP_ADDR, NGINX_POP3_ADDR, NGINX_UNAUTH_SMTP_ADDR, ROUTE_A_DOMAIN,
	ROUTE_AUTH_ONLY_DOMAIN, ROUTE_B_DOMAIN, ROUTE_IMAP_ONLY_DOMAIN, ROUTE_SMTP_ONLY_DOMAIN, ROUTE_SPLIT_DOMAIN,
	get_env,
};

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
