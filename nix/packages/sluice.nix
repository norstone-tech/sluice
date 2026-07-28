{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "sluice";
  version = "0.1.0";

  src = lib.cleanSource ../..;

  cargoLock.lockFile = ../../Cargo.lock;

  # The test suite spins up a real nginx process and binds real loopback sockets end-to-end
  # (see tests/support/); that's not something to run inside the Nix build sandbox. CI runs it
  # instead (.github/workflows/ci.yml).
  doCheck = false;

  meta = {
    description = "Flow-control for ngx_mail_auth_http_module";
    mainProgram = "sluice";
  };
}
