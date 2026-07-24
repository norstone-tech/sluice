# sluice

![CI](https://github.com/norstone-tech/sluice/actions/workflows/ci.yml/badge.svg)

Simple flow-control for [`ngx_mail_auth_http_module`](https://nginx.org/en/docs/mail/ngx_mail_auth_http_module.html)

nginx's mail proxy asks sluice one question per SMTP session, "which upstream server should this go to?", and sluice answers it from a small domain-to-server routing table.

## How routing works

sluice exposes `GET /auth`, the endpoint nginx's `auth_http` directive calls. The domain used to look an entry up in `proxy_map`, in priority order:

1. **The authenticated user's own domain** — for authenticated connections (nginx sends this at `AUTH` time, before `MAIL FROM`/`RCPT TO` even exist).
2. **The mail recipient's domain** (`RCPT TO`) — for unauthenticated connections.
3. **The mail sender's domain** (`MAIL FROM`), as a fallback if the recipient's domain isn't managed by this server.

Authenticated connections never fall back to the recipient's or sender's domain - only the authenticated identity's own domain is trusted for routing. Invalid or unparseable addresses are rejected outright rather than silently falling through to the next priority level.

## Configuration

### sluice

sluice takes a single cli argument: the path to a TOML config file. (`sluice path/to/config.toml`)

```toml
bind = ["127.0.0.1:8080"]
log_filter = "sluice=info,warn"

[proxy_map]
"example.com" = "10.0.0.5:25"
"example.org" = "10.0.0.6:25"
```

- `bind` — addresses the HTTP server listens on. Each entry is either `host:port` or `unix://path/to.sock`.
- `log_filter` — a [`tracing-subscriber` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directive.
- `proxy_map` — the routing table described above.

Sending `SIGHUP` reloads the config in place (no restart, no dropped connections) and re-reads the same file path the process was started with.

### nginx

nginx needs to be built with mail proxy support (`--with-mail`) — not every nginx build includes it; see the NixOS section under Deployment for how to get that on NixOS specifically. Point `auth_http` at wherever sluice is listening:

```nginx
mail {
    server_name mail.example.com;
    auth_http http://127.0.0.1:8080/auth;
    proxy_smtp_auth on;
    proxy_pass_error_message on;

    server {
        listen 25;
        protocol smtp;
        smtp_auth login plain;
    }
}
```

The two `proxy_*` directives matter more here than they would with a typical mail proxy setup, and neither is nginx's default:

**`proxy_smtp_auth on;`** (default `off`, [`ngx_mail_proxy_module`](https://nginx.org/en/docs/mail/ngx_mail_proxy_module.html)) — **sluice never checks the AUTH credentials itself.** It only routes based on domain, and treats any non-`none` `Auth-Method` as sufficient to look up by the authenticated user's domain. The upstream server is what actually has to validate the password, which means nginx has to replay the client's `AUTH` command to it - without this directive, every upstream sees an unauthenticated connection regardless of what the client sent. Since sluice's `/auth` response never sets `Auth-User`/`Auth-Pass`, nginx replays the client's own original credentials to the backend unchanged (per [`ngx_mail_auth_http_module`](https://nginx.org/en/docs/mail/ngx_mail_auth_http_module.html): an `Auth-User`/`Auth-Pass` in sluice's response would override them, but sluice doesn't send either). One side effect: `xclient` is `on` by default, and `proxy_smtp_auth on` suppresses its `LOGIN=` parameter — the real `AUTH` command is the thing that carries the identity to the backend once this is enabled, not XCLIENT.

**`proxy_pass_error_message on;`** (also default `off`) matters for the same reason. nginx's docs frame a backend rejecting an already-"successful" auth as an edge case ("usually...means some internal error has occurred") worth surfacing only for quirky POP3 servers - but with sluice, that's backwards: since sluice never validates the password, a wrong password is *only* ever caught by the backend rejecting the replayed `AUTH`, which is the normal path here, not an anomaly. Without this directive, the client just sees an opaque nginx error instead of the backend's actual "invalid credentials" message.

## Deployment

### NixOS

Create a `sluice.nix` file:

```nix
{ config, pkgs, ... }:
let
  sluice-src = builtins.fetchGit {
    url = "https://github.com/norstone-tech/sluice.git";
    rev = "PUT_COMMIT_SHA_HERE";
  };
in {
  imports = [ "${sluice-src}/module.nix" ];

  services.sluice = {
    enable = true;
    proxyMap."example.com" = "10.0.0.5:25";
  };
}
```

Then import it into your `configuration.nix`:

```nix
{
  imports = [ ./sluice.nix ];
}
```

See `module.nix` for the full option list (`bind`, `logFilter`, `proxyMap`, `package`).

nginx itself isn't built with mail proxy support by default on NixOS — same as upstream nixpkgs, `services.nginx` needs the module explicitly enabled. The directives below are the same ones explained in the nginx section above:

```nix
services.nginx.package = pkgs.nginx.override { withMail = true; };

services.nginx.mailConfig = ''
  server_name mail.example.com;
  auth_http http://127.0.0.1:8080/auth;
  proxy_smtp_auth on;
  proxy_pass_error_message on;

  server {
    listen 25;
    protocol smtp;
    smtp_auth login plain;
  }
'';
```

Point `auth_http` at wherever `services.sluice.bind` is listening.

### Other platforms

`default.nix` is a standard `rustPlatform.buildRustPackage` derivation (`nix-build -E 'with import <nixpkgs> {}; callPackage ./default.nix {}'`), or build with plain `cargo build --release` and run the resulting binary with a config path as its only argument.

## Development

`shell.nix` provides the full dev environment (Rust toolchain, nginx built with mail proxy support, `nixfmt`, `cargo-llvm-cov`):

```sh
nix-shell
cargo test    # runs a real nginx + real sluice HTTP server e2e suite, see tests/support/
cargo clippy --all-targets -- -D warnings
cargo fmt --check
nixfmt --check *.nix
```

CI (`.github/workflows/ci.yml`) runs all of the above on every push/PR.
