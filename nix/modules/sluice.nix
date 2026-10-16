{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.sluice;

  tomlFormat = pkgs.formats.toml { };

  settings = {
    bind = cfg.bind;
    log_filter = cfg.logFilter;
    proxy_map = cfg.proxyMap;
  };

  configFile = tomlFormat.generate "sluice.toml" settings;
in
{
  options.services.sluice = {
    enable = lib.mkEnableOption "sluice, the auth_http backend for nginx's mail proxy";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../packages/sluice.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../packages/sluice.nix {}";
      description = "The sluice package to use.";
    };

    bind = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "127.0.0.1:8080" ];
      description = ''
        Addresses sluice's HTTP server listens on. Each entry is either `host:port` or
        `unix://path/to.sock`. Point nginx's `auth_http` directive at whichever of these
        it should reach (e.g. `http://127.0.0.1:8080/auth`).
      '';
      example = [
        "127.0.0.1:8080"
        "unix:///run/sluice/sluice.sock"
      ];
    };

    logFilter = lib.mkOption {
      type = lib.types.str;
      default = "sluice=info,abpl=info,warn";
      description = ''
        logging verbosity of sluice and its dependant libraries. Some example configs:

        - `sluice=info,abpl=info,warn`: Sluice will log on startup, reload, and every
          time a lookup event happens.
        - `warn`: Sluice will log when a lookup is unsuccessful.
        - `error`: Silence... unless something bad happens.
      '';
      example = "sluice=debug,warn";
    };

    proxyMap = lib.mkOption {
      type = lib.types.attrsOf (lib.types.either lib.types.str (lib.types.attrsOf lib.types.str));
      default = { };
      description = ''
        Map from a mailbox domain or a client IP address to the upstream server address
        (`host:port`) sluice tells nginx to proxy that session to.

        Keys must be either an IP address (IPv4 or IPv6, not bracketed) or a valid domain name;
        anything else is a configuration error. Domains are case-insensitive and
        internationalized domains must be written in punycode. IPv4-mapped IPv6 addresses
        (`::ffff:a.b.c.d`) are treated as the plain IPv4 address.

        The entry used is the first match, in priority order, of:
        - the authenticated user's own domain (authenticated connections only)
        - the mail recipient's domain (unauthenticated connections only)
        - the client's IP address, as reported by nginx's `Client-IP`
        - the unauthenticated mail sender's domain (unauthenticated connections only).

        IP entries only ever match the connecting client's own address, never an IP written into
        an e-mail address. They are meant for trusted outbound relays: any recipient domain sluice
        doesn't manage, from a listed client IP, is routed to that IP's upstream. Only list
        addresses whose upstream you trust to handle that mail.

        If nginx runs behind a proxy or load balancer, the client address is the proxy's unless
        nginx uses `proxy_protocol` together with `set_real_ip_from`. Without that, an IP entry
        matches every client behind the proxy.

        A domain's value is either a single address+port, which assumes nginx is only proxying
        a single procol used for every protocol, or an attribute set mapping a protocol to its
        own address+port. Valid protocol names are `smtp`, `smtp_authenticated`, `imap` and
        `pop3`.

        Since nginx doesn't tell sluice which port it is proxying, and some mail servers may
        behave differently based on which port SMTP is using (i.e. 25 vs 587) sluice provides
        a workaround via `smtp_authenticated`. This way you can proxy authenticated
        connections to port 587, and unauthenticated connections to port 25. If
        `smtp_authenticated` isn't specified, then the `smtp` entry will be used for
        authenticated connections too.
      '';
      example = {
        "example.com" = "10.0.0.5:25";
        # a trusted outbound relay, by client IP
        "192.0.2.10" = "10.0.0.9:25";
        "example.org" = {
          smtp = "10.0.0.6:25";
          smtp_authenticated = "10.0.0.6:587";
          imap = "10.0.0.6:143";
        };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.sluice = {
      isSystemUser = true;
      group = "sluice";
      description = "sluice service user";
    };

    users.groups.sluice = { };

    systemd.services.sluice = {
      description = "sluice: simple auth_http backend for nginx's mail proxy";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      before = [ "nginx.service" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "notify-reload";
        User = "sluice";
        Group = "sluice";
        ExecStart = "${cfg.package}/bin/sluice ${configFile}";
        Restart = "on-failure";
        RestartSec = "5s";
        # Hardening
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
      };
    };
  };
}
