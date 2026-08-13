use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr as NetSocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use abpl::app::axum::HotReloadingAxumService;
use abpl::sync::MithridatistLockResult as _;
use abpl::types::http::SocketAddr as HttpSocketAddr;
use sluice::config::{MailProtocol, NetSocketAddrs, SluiceState};
use sluice::http;

use super::nginx::NginxHandle;

/// Test domains routed through the fixture's proxy_map. `ROUTE_A` and `ROUTE_B` each
/// resolve to their own dummy upstream so tests can assert which one a session landed on.
pub const ROUTE_A_DOMAIN: &str = "routea.test";
pub const ROUTE_B_DOMAIN: &str = "routeb.test";
/// Mapped with the per-protocol (`NetSocketAddrs::ByProtocol`) form rather than a single
/// address: unauthenticated SMTP goes to upstream A, authenticated SMTP to upstream B. This
/// is the fixture's stand-in for the real "port 25 vs port 587" split the variant exists for.
pub const ROUTE_SPLIT_DOMAIN: &str = "routesplit.test";
/// Also mapped per-protocol, but *only* for IMAP - so any SMTP session naming this domain is
/// a managed domain that has no upstream for the protocol in play (`ProtocolLookupFailed`).
pub const ROUTE_IMAP_ONLY_DOMAIN: &str = "routeimaponly.test";
/// Mapped per-protocol with an `smtp` entry but deliberately *no* `smtp_authenticated` one,
/// to exercise `NetSocketAddrs::get`'s one-way fallback.
pub const ROUTE_SMTP_ONLY_DOMAIN: &str = "routesmtponly.test";
/// The mirror of `ROUTE_SMTP_ONLY_DOMAIN`: an `smtp_authenticated` entry and no `smtp` one,
/// to prove that fallback does *not* run in the other direction.
pub const ROUTE_AUTH_ONLY_DOMAIN: &str = "routeauthonly.test";

// Every fixture service gets its own address on the 127.0.0.0/8 loopback range instead of
// sharing 127.0.0.1 on dynamically picked ports - simpler to read/debug, and no
// bind-then-release TOCTOU race against whatever else is running on the machine.
//
// A distinct loopback address is not on its own enough to avoid collisions, though: a
// process bound to a *wildcard* address (`*:8080`, as opposed to `127.0.0.1:8080`) occupies
// that port on every loopback address too. Ports here are therefore chosen to be uncommon as
// well as separated by address - notably *not* 8080, which a locally-running mail server or
// dev HTTP server is very likely to have claimed wildcard.
const fn loopback(host: u8, port: u16) -> NetSocketAddr {
	NetSocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, host), port))
}
const DUMMY_UPSTREAM_A_ADDR: NetSocketAddr = loopback(2, 32500);
const DUMMY_UPSTREAM_B_ADDR: NetSocketAddr = loopback(3, 32500);
// Mailbox-protocol upstreams are separate listeners rather than more entries pointing at A/B,
// because the dialect an upstream greets in is fixed before it has read anything - see
// `Dialect`. Being distinct addresses also sharpens the routing assertions: a session landing
// here can only have got here by looking up the `imap`/`pop3` entry specifically.
const DUMMY_IMAP_UPSTREAM_ADDR: NetSocketAddr = loopback(9, 32143);
const DUMMY_POP3_UPSTREAM_ADDR: NetSocketAddr = loopback(10, 32110);
const SLUICE_HTTP_AUTH_ADDR: NetSocketAddr = loopback(4, 38080);
pub const NGINX_UNAUTH_SMTP_ADDR: NetSocketAddr = loopback(5, 2525);
pub const NGINX_AUTH_SMTP_ADDR: NetSocketAddr = loopback(6, 2525);
pub const NGINX_IMAP_ADDR: NetSocketAddr = loopback(7, 2143);
pub const NGINX_POP3_ADDR: NetSocketAddr = loopback(8, 2110);

/// End-to-end fixture: two dummy upstream "mail servers", sluice's real HTTP auth server
/// (the actual `http::build_router()`, not a stand-in), and a real nginx mail proxy wired
/// up to route between them via sluice. Shared across parallel tests the same way
/// `NginxHandle` is; see that module for why `Mutex<Weak<_>>` instead of `OnceLock`.
pub struct SluiceEnv {
	axum: HotReloadingAxumService<SluiceState>,
	_nginx: NginxHandle,
	_upstream_a: DummyUpstream,
	_upstream_b: DummyUpstream,
	_upstream_imap: DummyUpstream,
	_upstream_pop3: DummyUpstream,
	upstream_hits: Mutex<Receiver<UpstreamHit>>,
	// `upstream_hits` is a single mpsc queue shared by every test using this env: a hit sent
	// while two tests are both waiting on it can be delivered to whichever `recv` happens to
	// be blocked first, not necessarily the test whose own connection produced it. Holding
	// this for the "run one SMTP session, then check what it produced" span serializes that
	// part (and only that part - env setup/teardown still stays shared) so hits can't cross
	// between tests.
	session_lock: Mutex<()>,
}

pub struct UpstreamHit {
	pub upstream: &'static str,
	pub lines: Vec<String>,
}

static ENV: Mutex<Weak<SluiceEnv>> = Mutex::new(Weak::new());

pub fn get_env() -> Arc<SluiceEnv> {
	let mut slot = ENV.lock().mithridate();
	if let Some(env) = slot.upgrade() {
		return env;
	}
	let env = Arc::new(SluiceEnv::spawn());
	*slot = Arc::downgrade(&env);
	env
}

impl SluiceEnv {
	fn spawn() -> Self {
		let (hits_tx, hits_rx) = mpsc::channel();
		let upstream_a = DummyUpstream::spawn("route-a", DUMMY_UPSTREAM_A_ADDR, Dialect::Smtp, hits_tx.clone());
		let upstream_b = DummyUpstream::spawn("route-b", DUMMY_UPSTREAM_B_ADDR, Dialect::Smtp, hits_tx.clone());
		let upstream_imap =
			DummyUpstream::spawn("route-imap", DUMMY_IMAP_UPSTREAM_ADDR, Dialect::Imap, hits_tx.clone());
		let upstream_pop3 = DummyUpstream::spawn("route-pop3", DUMMY_POP3_UPSTREAM_ADDR, Dialect::Pop3, hits_tx);

		let mut proxy_map = HashMap::new();
		proxy_map.insert(
			ROUTE_A_DOMAIN.to_string(),
			NetSocketAddrs::Single(DUMMY_UPSTREAM_A_ADDR),
		);
		proxy_map.insert(
			ROUTE_B_DOMAIN.to_string(),
			NetSocketAddrs::Single(DUMMY_UPSTREAM_B_ADDR),
		);
		proxy_map.insert(
			ROUTE_SPLIT_DOMAIN.to_string(),
			NetSocketAddrs::ByProtocol(HashMap::from([
				(MailProtocol::Smtp, DUMMY_UPSTREAM_A_ADDR),
				(MailProtocol::SmtpAuthenticated, DUMMY_UPSTREAM_B_ADDR),
				(MailProtocol::Pop3, DUMMY_POP3_UPSTREAM_ADDR),
			])),
		);
		proxy_map.insert(
			ROUTE_IMAP_ONLY_DOMAIN.to_string(),
			NetSocketAddrs::ByProtocol(HashMap::from([(MailProtocol::Imap, DUMMY_IMAP_UPSTREAM_ADDR)])),
		);
		proxy_map.insert(
			ROUTE_SMTP_ONLY_DOMAIN.to_string(),
			NetSocketAddrs::ByProtocol(HashMap::from([(MailProtocol::Smtp, DUMMY_UPSTREAM_A_ADDR)])),
		);
		proxy_map.insert(
			ROUTE_AUTH_ONLY_DOMAIN.to_string(),
			NetSocketAddrs::ByProtocol(HashMap::from([(
				MailProtocol::SmtpAuthenticated,
				DUMMY_UPSTREAM_A_ADDR,
			)])),
		);

		let mut axum = HotReloadingAxumService::new(SluiceState { proxy_map }, |_| http::build_router());
		axum.bind_sockets([HttpSocketAddr::Ip(SLUICE_HTTP_AUTH_ADDR)])
			.expect("failed to bind sluice http auth server");

		let config = format!(
			r#"
daemon off;
worker_processes 1;
error_log stderr info;
pid {pid_path};

events {{
    worker_connections 64;
}}

mail {{
    server_name localhost;
    auth_http http://{SLUICE_HTTP_AUTH_ADDR}/auth;
    xclient off;

    server {{
        listen {NGINX_UNAUTH_SMTP_ADDR};
        protocol smtp;
        smtp_auth none;
    }}

    server {{
        listen {NGINX_AUTH_SMTP_ADDR};
        protocol smtp;
        smtp_auth login plain;
    }}

    server {{
        listen {NGINX_IMAP_ADDR};
        protocol imap;
    }}

    server {{
        listen {NGINX_POP3_ADDR};
        protocol pop3;
    }}
}}
"#,
			pid_path = std::env::temp_dir()
				.join(format!("sluice-test-nginx-{}.pid", std::process::id()))
				.display(),
		);
		let nginx = NginxHandle::spawn(
			&config,
			&[
				NGINX_UNAUTH_SMTP_ADDR,
				NGINX_AUTH_SMTP_ADDR,
				NGINX_IMAP_ADDR,
				NGINX_POP3_ADDR,
			],
		);

		Self {
			axum,
			_nginx: nginx,
			_upstream_a: upstream_a,
			_upstream_b: upstream_b,
			_upstream_imap: upstream_imap,
			_upstream_pop3: upstream_pop3,
			upstream_hits: Mutex::new(hits_rx),
			session_lock: Mutex::new(()),
		}
	}

	/// Acquires exclusive use of the shared upstream-hit tracking for one SMTP session.
	/// Hold this for the full span from connecting through checking (or asserting the
	/// absence of) the resulting hit - see the note on `session_lock` for why.
	pub fn lock_session(&self) -> impl Drop {
		self.session_lock.lock().mithridate()
	}

	/// Waits for one of the dummy upstreams to report a completed session (i.e. the client
	/// connection to nginx has been closed, which closes nginx's connection to whichever
	/// upstream it picked). Call with `lock_session()` held.
	pub fn recv_upstream_hit(&self, timeout: Duration) -> UpstreamHit {
		self.upstream_hits
			.lock()
			.mithridate()
			.recv_timeout(timeout)
			.expect("no upstream received a proxied connection in time")
	}

	/// Asserts that no upstream was contacted within `timeout`. Used to confirm a rejected
	/// session was never proxied anywhere, not just that it got some error response.
	pub fn assert_no_upstream_hit(&self, timeout: Duration) {
		match self.upstream_hits.lock().mithridate().recv_timeout(timeout) {
			Ok(hit) => panic!("expected no upstream to be contacted, but {} was", hit.upstream),
			Err(mpsc::RecvTimeoutError::Timeout) => {},
			Err(mpsc::RecvTimeoutError::Disconnected) => panic!("upstream hit channel disconnected unexpectedly"),
		}
	}

	/// Calls sluice's `/auth` endpoint directly, bypassing nginx, with exactly the headers
	/// given. nginx only ever sends `Auth-Protocol` values it itself supports, so this is the
	/// only way to reach `auth()`'s malformed/missing-protocol arms - everything else should
	/// go through a real SMTP session against nginx instead. Returns the response headers,
	/// keyed lowercase.
	pub fn raw_auth_request(&self, headers: &[(&str, &str)]) -> HashMap<String, String> {
		let mut request = format!("GET /auth HTTP/1.1\r\nHost: {SLUICE_HTTP_AUTH_ADDR}\r\nConnection: close\r\n");
		for (name, value) in headers {
			request.push_str(&format!("{name}: {value}\r\n"));
		}
		request.push_str("\r\n");

		let mut stream = TcpStream::connect(SLUICE_HTTP_AUTH_ADDR).expect("failed to connect to sluice http server");
		stream
			.write_all(request.as_bytes())
			.expect("failed to write to sluice http server");

		let mut reader = BufReader::new(stream);
		let mut status_line = String::new();
		reader
			.read_line(&mut status_line)
			.expect("failed to read sluice status line");
		assert!(
			status_line.starts_with("HTTP/1.1 200"),
			"sluice's /auth should always answer 200 and put the verdict in headers, got: {status_line}"
		);

		let mut response_headers = HashMap::new();
		loop {
			let mut line = String::new();
			reader.read_line(&mut line).expect("failed to read sluice header line");
			let line = line.trim_end_matches(['\r', '\n']);
			if line.is_empty() {
				return response_headers;
			}
			let (name, value) = line.split_once(':').expect("malformed header line from sluice");
			response_headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
		}
	}
}

impl Drop for SluiceEnv {
	fn drop(&mut self) {
		self.axum.stop();
		// _nginx's own Drop handles the nginx shutdown.
	}
}

/// The wire dialect a `DummyUpstream` speaks. nginx runs a real proxy handshake against the
/// upstream it picks, and aborts the client session if that handshake fails - so an upstream
/// serving IMAP has to greet like an IMAP server, not an SMTP one. The greeting has to be
/// sent before the upstream has seen a single byte, which is why this is fixed per instance
/// rather than sniffed per connection.
#[derive(Clone, Copy)]
enum Dialect {
	Smtp,
	Imap,
	Pop3,
}

impl Dialect {
	fn greeting(self) -> &'static str {
		match self {
			Self::Smtp => "220 dummy ESMTP\r\n",
			Self::Imap => "* OK dummy IMAP ready\r\n",
			Self::Pop3 => "+OK dummy POP3 ready\r\n",
		}
	}

	/// `command_number` is 1-based within the connection, and `tag` is the first token of the
	/// first command line.
	fn reply_to(self, command_number: usize, tag: &str) -> String {
		match self {
			Self::Smtp => "250 OK\r\n".to_string(),
			// nginx doesn't proxy IMAP LOGIN as one line: it uses literals, sending
			// `a1 LOGIN {17}`, then the login, then the password, as three separate lines. The
			// first two want a bare continuation and only the last gets a tagged completion -
			// answering the tagged form too early makes nginx abort with
			// "* BAD internal server error". Recorded from real nginx rather than its docs.
			Self::Imap if command_number < 3 => "+ \r\n".to_string(),
			Self::Imap => format!("{tag} OK logged in\r\n"),
			Self::Pop3 => "+OK\r\n".to_string(),
		}
	}
}

/// A minimal stand-in mail server: greets in its `Dialect`, accepts every command, and
/// reports the lines it received once the client (nginx, relaying the real client) hangs
/// up. Good enough to prove sluice routed to *this* upstream and forwarded MAIL FROM/RCPT
/// TO correctly - it's not meant to behave like a real mail server beyond that.
///
/// Its accept loop runs on a dedicated thread; `Drop` signals it to stop and unblocks the
/// blocking `accept()` with a throwaway connection to itself, then joins the thread so the
/// listening socket is actually released (not just orphaned) before returning - fixed
/// addresses mean a leaked listener from one generation would stop the next from binding.
struct DummyUpstream {
	addr: NetSocketAddr,
	stop: Arc<AtomicBool>,
	handle: Option<JoinHandle<()>>,
}

impl DummyUpstream {
	fn spawn(name: &'static str, addr: NetSocketAddr, dialect: Dialect, hits: Sender<UpstreamHit>) -> Self {
		let listener =
			TcpListener::bind(addr).unwrap_or_else(|err| panic!("failed to bind dummy upstream {addr}: {err}"));
		let stop = Arc::new(AtomicBool::new(false));
		let thread_stop = stop.clone();
		let handle = thread::spawn(move || {
			for stream in listener.incoming() {
				if thread_stop.load(Ordering::Acquire) {
					return;
				}
				let Ok(stream) = stream else { continue };
				let hits = hits.clone();
				thread::spawn(move || handle_dummy_upstream_connection(name, dialect, stream, hits));
			}
		});
		Self {
			addr,
			stop,
			handle: Some(handle),
		}
	}
}

impl Drop for DummyUpstream {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Release);
		// Unblocks the accept loop's blocking `listener.incoming()` call.
		let _ = TcpStream::connect(self.addr);
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

fn handle_dummy_upstream_connection(
	name: &'static str,
	dialect: Dialect,
	mut stream: TcpStream,
	hits: Sender<UpstreamHit>,
) {
	if stream.write_all(dialect.greeting().as_bytes()).is_err() {
		return;
	}
	let reader = BufReader::new(stream.try_clone().expect("failed to clone dummy upstream stream"));
	let mut lines = Vec::new();
	let mut tag = String::from("*");
	for line in reader.lines() {
		let Ok(line) = line else { break };
		if lines.is_empty() {
			tag = line.split_whitespace().next().unwrap_or("*").to_string();
		}
		let reply = dialect.reply_to(lines.len() + 1, &tag);
		lines.push(line);
		if stream.write_all(reply.as_bytes()).is_err() {
			break;
		}
	}
	let _ = hits.send(UpstreamHit { upstream: name, lines });
}
