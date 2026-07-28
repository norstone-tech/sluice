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
      description = "logging verbosity of sluice and its dependant libraries";
      example = "sluice=debug,warn";
    };

    proxyMap = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = ''
        Map from a mailbox domain to the upstream SMTP server address (`host:port`) sluice
        tells nginx to proxy that session to.

        The domain used to look up an entry is, in priority order: the authenticated user's
        own domain (authenticated connections only), the mail recipient's domain, then the
        unauthenticated mail sender's domain.
      '';
      example = {
        "example.com" = "10.0.0.5:25";
        "example.org" = "10.0.0.6:25";
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
