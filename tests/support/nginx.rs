use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A running nginx instance started from an arbitrary config. Killed (SIGTERM, not
/// SIGKILL, so the master can shut its workers down cleanly) when dropped.
pub struct NginxHandle {
	child: Child,
}

impl NginxHandle {
	/// Spawns nginx with the given config, waiting until every address in `wait_addrs` is
	/// accepting connections (or panicking if nginx exits early / times out).
	pub fn spawn(config: &str, wait_addrs: &[SocketAddr]) -> Self {
		let config_path = std::env::temp_dir().join(format!("sluice-test-nginx-{}.conf", std::process::id()));
		std::fs::write(&config_path, config).expect("failed to write nginx test config");

		let mut child = Command::new("nginx")
			.args(["-e", "stderr"])
			.args(["-c", config_path.to_str().expect("config path is valid UTF-8")])
			.spawn()
			.expect("failed to spawn nginx");

		for &addr in wait_addrs {
			wait_until_listening(&mut child, addr, Duration::from_secs(5));
		}

		Self { child }
	}
}

impl Drop for NginxHandle {
	fn drop(&mut self) {
		// `Child::kill()` is SIGKILL, which doesn't give the nginx master a chance to shut
		// its worker processes down; SIGTERM (fast shutdown) lets it clean up after itself.
		unsafe {
			libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
		}
		let _ = self.child.wait();
	}
}

fn wait_until_listening(child: &mut Child, addr: SocketAddr, timeout: Duration) {
	let deadline = Instant::now() + timeout;
	loop {
		if TcpStream::connect(addr).is_ok() {
			return;
		}
		if let Some(status) = child.try_wait().expect("failed to poll nginx status") {
			panic!("nginx exited early with {status} before listening on {addr}");
		}
		if Instant::now() >= deadline {
			panic!("nginx did not start listening on {addr} within {timeout:?}");
		}
		std::thread::sleep(Duration::from_millis(20));
	}
}
