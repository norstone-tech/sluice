//! Config-file parsing, exercised through `abpl`'s real `parse_toml_file` rather than a
//! direct `toml::from_str`, so these cover the same path `main.rs` uses at startup.
//!
//! `NetSocketAddrs` has a hand-written `Deserialize` specifically so that a bad protocol name
//! reports itself usefully - `#[serde(untagged)]` buffers the value and discards each
//! variant's error, which collapses the message to "data did not match any variant" pointing
//! at the whole table. That's a silent regression if anyone ever swaps the impl back for a
//! derive, hence asserting on the message text here.

use std::net::SocketAddr;
use std::path::PathBuf;

use abpl::app::config::parse_toml_file;
use sluice::config::{MailProtocol, SluiceConfig};

/// Writes `contents` to a uniquely-named temp file and parses it. The file is left in place
/// on failure so a broken case can be inspected by hand.
fn parse(name: &str, contents: &str) -> Result<SluiceConfig, String> {
	let path: PathBuf = std::env::temp_dir().join(format!("sluice-config-test-{}-{name}.toml", std::process::id()));
	std::fs::write(&path, contents).expect("failed to write test config");
	// `{:-}` renders abpl's full error chain; the bare `Display` is only the outermost layer,
	// which would hide the TOML error underneath.
	let parsed = parse_toml_file::<SluiceConfig, _>(Some(path.clone())).map_err(|err| format!("{err:-}"));
	if parsed.is_ok() {
		let _ = std::fs::remove_file(&path);
	}
	parsed
}

const BOILERPLATE: &str = "bind = [\"127.0.0.1:8080\"]\nlog_filter = \"sluice=info,warn\"\n";

/// Both `proxy_map` value forms parse from one file, and each resolves the way the routing
/// table expects - including the `smtp_authenticated` -> `smtp` fallback and its deliberate
/// one-way-ness. The e2e suite proves nginx drives this correctly; this proves the config
/// shape an operator actually writes produces those lookups in the first place.
#[test]
fn parses_both_proxy_map_forms() {
	let config = parse(
		"both-forms",
		&format!(
			r#"{BOILERPLATE}
[proxy_map]
"single.test" = "10.0.0.5:25"

[proxy_map."split.test"]
smtp = "10.0.0.6:25"
smtp_authenticated = "10.0.0.6:587"
imap = "10.0.0.6:143"

[proxy_map."smtponly.test"]
smtp = "10.0.0.7:25"

[proxy_map."authonly.test"]
smtp_authenticated = "10.0.0.8:587"
"#
		),
	)
	.expect("config with both proxy_map forms should parse");

	let get = |domain: &str, protocol| {
		config
			.proxy_map
			.get(domain)
			.unwrap_or_else(|| panic!("{domain} missing from proxy_map"))
			.get(&protocol)
			.map_err(|err| format!("{err:-}"))
	};
	let addr = |s: &str| s.parse::<SocketAddr>().expect("test address is valid");

	// A single address answers for every protocol.
	assert_eq!(get("single.test", MailProtocol::Smtp), Ok(addr("10.0.0.5:25")));
	assert_eq!(
		get("single.test", MailProtocol::SmtpAuthenticated),
		Ok(addr("10.0.0.5:25"))
	);
	assert_eq!(get("single.test", MailProtocol::Imap), Ok(addr("10.0.0.5:25")));

	// An explicit per-protocol entry wins over the fallback.
	assert_eq!(get("split.test", MailProtocol::Smtp), Ok(addr("10.0.0.6:25")));
	assert_eq!(
		get("split.test", MailProtocol::SmtpAuthenticated),
		Ok(addr("10.0.0.6:587"))
	);
	assert_eq!(get("split.test", MailProtocol::Imap), Ok(addr("10.0.0.6:143")));

	// No `smtp_authenticated` entry falls back to the `smtp` one...
	assert_eq!(
		get("smtponly.test", MailProtocol::SmtpAuthenticated),
		Ok(addr("10.0.0.7:25"))
	);
	// ...but never the reverse, and never across unrelated protocols.
	assert!(get("authonly.test", MailProtocol::Smtp).is_err());
	assert!(get("smtponly.test", MailProtocol::Imap).is_err());
}

/// A misspelled protocol name must name the offending key and list the valid options. Both
/// halves matter: the message is what tells an operator what to type, and the span is what
/// tells them where.
#[test]
fn rejects_unknown_protocol_key_with_a_useful_message() {
	let err = parse(
		"bad-protocol",
		&format!(
			r#"{BOILERPLATE}
[proxy_map."example.test"]
smtp_auth = "10.0.0.6:587"
"#
		),
	)
	.expect_err("an unknown protocol key should be rejected");

	assert!(
		err.contains("smtp_authenticated") && err.contains("pop3"),
		"error should list the valid protocol names, got: {err}"
	);
	assert!(
		err.contains("smtp_auth ="),
		"error should point at the offending key rather than the enclosing table, got: {err}"
	);
	assert!(
		!err.contains("did not match any variant"),
		"the untagged-enum message means the hand-written Deserialize was lost, got: {err}"
	);
}

/// The other arm of the visitor: a value that is a string but not an address reports the
/// address problem, rather than being reported as "not a table either".
#[test]
fn rejects_malformed_upstream_address() {
	let err = parse(
		"bad-address",
		&format!("{BOILERPLATE}\n[proxy_map]\n\"example.test\" = \"not-an-address\"\n"),
	)
	.expect_err("a malformed address should be rejected");

	assert!(
		err.contains("invalid socket address syntax"),
		"error should describe the address problem, got: {err}"
	);
	assert!(
		!err.contains("did not match any variant"),
		"the untagged-enum message means the hand-written Deserialize was lost, got: {err}"
	);
}
