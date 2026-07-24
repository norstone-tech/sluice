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
use sluice::config::SluiceState;
use sluice::http;

use super::nginx::NginxHandle;

/// Test domains routed through the fixture's proxy_map. `ROUTE_A` and `ROUTE_B` each
/// resolve to their own dummy upstream so tests can assert which one a session landed on.
pub const ROUTE_A_DOMAIN: &str = "routea.test";
pub const ROUTE_B_DOMAIN: &str = "routeb.test";

// Every fixture service gets its own address on the 127.0.0.0/8 loopback range instead of
// sharing 127.0.0.1 on dynamically picked ports - simpler to read/debug, and no
// bind-then-release TOCTOU race against whatever else is running on the machine.
const fn loopback(host: u8, port: u16) -> NetSocketAddr {
	NetSocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, host), port))
}
const DUMMY_UPSTREAM_A_ADDR: NetSocketAddr = loopback(2, 2500);
const DUMMY_UPSTREAM_B_ADDR: NetSocketAddr = loopback(3, 2500);
const SLUICE_HTTP_AUTH_ADDR: NetSocketAddr = loopback(4, 8080);
pub const NGINX_UNAUTH_SMTP_ADDR: NetSocketAddr = loopback(5, 2525);
pub const NGINX_AUTH_SMTP_ADDR: NetSocketAddr = loopback(6, 2525);

/// End-to-end fixture: two dummy upstream "mail servers", sluice's real HTTP auth server
/// (the actual `http::build_router()`, not a stand-in), and a real nginx mail proxy wired
/// up to route between them via sluice. Shared across parallel tests the same way
/// `NginxHandle` is; see that module for why `Mutex<Weak<_>>` instead of `OnceLock`.
pub struct SluiceEnv {
	axum: HotReloadingAxumService<SluiceState>,
	_nginx: NginxHandle,
	_upstream_a: DummyUpstream,
	_upstream_b: DummyUpstream,
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
		let upstream_a = DummyUpstream::spawn("route-a", DUMMY_UPSTREAM_A_ADDR, hits_tx.clone());
		let upstream_b = DummyUpstream::spawn("route-b", DUMMY_UPSTREAM_B_ADDR, hits_tx);

		let mut proxy_map = HashMap::new();
		proxy_map.insert(ROUTE_A_DOMAIN.to_string(), DUMMY_UPSTREAM_A_ADDR);
		proxy_map.insert(ROUTE_B_DOMAIN.to_string(), DUMMY_UPSTREAM_B_ADDR);

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
}}
"#,
			pid_path = std::env::temp_dir()
				.join(format!("sluice-test-nginx-{}.pid", std::process::id()))
				.display(),
		);
		let nginx = NginxHandle::spawn(&config, &[NGINX_UNAUTH_SMTP_ADDR, NGINX_AUTH_SMTP_ADDR]);

		Self {
			axum,
			_nginx: nginx,
			_upstream_a: upstream_a,
			_upstream_b: upstream_b,
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
}

impl Drop for SluiceEnv {
	fn drop(&mut self) {
		self.axum.stop();
		// _nginx's own Drop handles the nginx shutdown.
	}
}

/// A minimal stand-in SMTP server: greets, accepts every command with "250 OK", and
/// reports the lines it received once the client (nginx, relaying the real client) hangs
/// up. Good enough to prove sluice routed to *this* upstream and forwarded MAIL FROM/RCPT
/// TO correctly - it's not meant to behave like real SMTP beyond that.
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
	fn spawn(name: &'static str, addr: NetSocketAddr, hits: Sender<UpstreamHit>) -> Self {
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
				thread::spawn(move || handle_dummy_upstream_connection(name, stream, hits));
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

fn handle_dummy_upstream_connection(name: &'static str, mut stream: TcpStream, hits: Sender<UpstreamHit>) {
	if stream.write_all(b"220 dummy ESMTP\r\n").is_err() {
		return;
	}
	let reader = BufReader::new(stream.try_clone().expect("failed to clone dummy upstream stream"));
	let mut lines = Vec::new();
	for line in reader.lines() {
		let Ok(line) = line else { break };
		lines.push(line);
		if stream.write_all(b"250 OK\r\n").is_err() {
			break;
		}
	}
	let _ = hits.send(UpstreamHit { upstream: name, lines });
}
