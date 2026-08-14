# sluice

![CI](https://github.com/norstone-tech/sluice/actions/workflows/ci.yml/badge.svg)

Simple flow-control for [`ngx_mail_auth_http_module`](https://nginx.org/en/docs/mail/ngx_mail_auth_http_module.html)

nginx's mail proxy asks sluice one question per SMTP session, "which upstream server should this go to?", and sluice
answers it from a small domain-to-server routing table.

## How routing works

sluice exposes `GET /auth`, the endpoint nginx's `auth_http` directive calls. The domain used to look an entry up in
`proxy_map`.
- For authenticated connections (SMTP `AUTH`, IMAP, POP3), the authenticated user's domain is used
    - Note that sluice requires e-mail address like usernames, and will reject stuff like `DOMAIN\USER`
    - Since nginx will ask sluice for an upstream before `MAIL FROM` and `RCPT TO` on an SMTP authenticated connection,
      there is no fallthrough to using these lookups. Sluice assumes that this limitation will always be true and
      doesn't even attempt to get the values for `MAIL FROM` and `RCPT TO` in that case.
- For unauthenticated connections, lookups are attempted in the following order of priority
    1. The mail recipient's domain (SMTP `RCPT TO`)
    2. The mail sender's domain (SMTP `MAIL FROM`)

> [!WARNING]
> 
> Sluice **does not** validate credentials nor mail. It's only job is to tell nginx what upstream server to route to.
> The **upstream server is still expected to validate credentials and take measures against fraudulent mail**.

## Configuration

### sluice

Sluice supports being started as a systemd service with `Type=notify-reload`. Reloading Sluice will reload the config
in-place without terminating any connections. Seamless.

It takes a single cli argument: the path to a TOML config file. (`sluice path/to/config.toml`)

```toml
# addresses the HTTP server listens on. Each entry is either `host:port` or `unix://path/to.sock`.
bind = ["127.0.0.1:8080"]
# logging level for sluice and its dependencies. By default sluice and its service library is at the `info` level with
# all other dependencies being at the `warn` level. Valid logging levels are, in order of verbosity: `error`, `warn`,
# `info`, `debug`, `trace`.
log_filter = "sluice=info,abpl=info,warn"

[proxy_map]
# Use this format if only 1 protocol is being proxied by nginx, like SMTP
"example.com" = "10.0.0.5:25"
"example.org" = "10.0.0.6:25"

# Use this format if multiple protocols are being proxied by nginx
[proxy_map."example.net"]
smtp = "10.0.0.7:25"
smtp_authenticated = "10.0.0.7:587"
imap = "10.0.0.7:143"
pop3 = "10.0.0.7:110"
```

### nginx

nginx needs to be built with mail proxy support. (`--with-mail`) Point `auth_http` at wherever sluice is listening:

```nginx
mail {
    server_name mail.example.com;
    auth_http http://127.0.0.1:8080/auth;
    proxy_smtp_auth on;
    proxy_pass_error_message on;

    # Of course you shouldn't actually accept plaintext logins in unencrypted connections, this is just a minimal
    # example.
    server {
        listen 25;
        protocol smtp;
        smtp_auth login plain;
    }
}
```

The two `proxy_*` directives matter more here than they would with a typical mail proxy setup, and neither is nginx's
default (see [`ngx_mail_proxy_module`](https://nginx.org/en/docs/mail/ngx_mail_proxy_module.html)):

- `proxy_smtp_auth on;` (default `off`)
    - As noted above, Sluice never checks the AUTH credentials itself. This means we need nginx to replay
      authentication commands to the upstream server.
- `proxy_pass_error_message on;` (default `off`)
    - Even though nginx's docs frame a backend rejecting an already "successful" auth as an edge case, again, Sluice
      doesn't actually do auth, we're still relying on the upstream server to do the authenticating. Therefore, we must
      pass along the actual rejection messages.

## Deployment

### NixOS

Pin sluice as a dependency with [`lon`](https://github.com/nix-community/lon) (`nix-shell -p lon`, or add it to your own `shell.nix`):

```sh
lon add github norstone-tech/sluice -r v0.1.2 --frozen
```

That writes/updates `lon.lock` and (re)generates `lon.nix` next to it. Swap `v0.1.2` for whichever tag or commit you want to track.

Then reference it from your `configuration.nix`. See [`nix/modules/sluice.nix`](nix/modules/sluice.nix) for the full option list.

```nix
{ pkgs, lib, ... }:
let
  sources = import ./lon.nix;
  sluice = import sources.sluice { inherit pkgs; };
in
{
  imports = [ sluice.nixosModules.sluice ];

  services.sluice = {
    enable = true;
    bind = [ "127.0.0.1:8080" ];
    proxyMap."example.com" = "10.0.0.5:25";

    # Or, to route a domain's sessions per protocol:
    proxyMap."example.net" = {
      smtp = "10.0.0.7:25";
      smtp_authenticated = "10.0.0.7:587";
      imap = "10.0.0.7:143";
    };
  };

  services.nginx = {
    enable = true;
    # NixOS doesn't build nginx with mail support by default. This is how to enable it.
    package = pkgs.nginx.override { withMail = true; };

    # nixpkgs doesn't provide "convenient" ways of configuring the mail proxy like it does with web proxies, so you'll
    # have to do something like this.
    appendConfig = ''
      mail {
        server_name mail.example.com;
        auth_http http://127.0.0.1:8080/auth;
        proxy_smtp_auth on;
        proxy_pass_error_message on;

        # Of course you shouldn't actually accept plaintext logins in unencrypted connections, this is just a minimal
        # example.
        server {
          listen 25;
          protocol smtp;
          smtp_auth login plain;
        }
      }
    '';
  };

}
```

### Other platforms

Sluice doesn't have any system dependencies beyond what Rust needs for its stdlib, so you can...
- `cargo install mail-sluice`
- Clone this repo, then `cargo build --release`

I don't provide prebuilds right now, though that may change in the future.

## Development

`shell.nix` provides the full dev environment, so after cloning this repo and `cd`-ing into it, you just need to run
`nix-shell` to get a shell with the Rust toolchain, a build of nginx with mail support, and other validation tools.

```sh
# get the tools
nix-shell

# # runs a real nginx + real sluice HTTP server e2e suite, see tests/support/
cargo test 

# linty linty
cargo clippy --all-targets -- -D warnings
cargo fmt --check
nixfmt --check *.nix nix/*/*.nix
```

CI (`.github/workflows/ci.yml`) runs all of the above on every push/PR.
