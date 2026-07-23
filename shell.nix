{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  name = "abpl-dev";

  packages = with pkgs; [
    # Rust toolchain
    cargo
    rustc
    rustfmt
    clippy

    # Code coverage. cargo-llvm-cov needs llvm-cov/llvm-profdata that match rustc's
    # bundled LLVM version (nixpkgs' rustc and llvmPackages.llvm both currently sit on
    # LLVM 21.1.8, so this is the pairing to keep in sync if either gets bumped).
    cargo-llvm-cov
    llvmPackages.llvm

    # Playground
    evcxr
    cargo-expand

    # rust-analyzer
    rust-analyzer

    # vscode complains if I don't have this?
    python3

    # uncomment below if we need native dependencies for some reason
    # rustPlatform.rustLibSrc
    # pkg-config
  ];
  # more stuff for rust-analyzer
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

  # cargo-llvm-cov looks for a rustup llvm-tools-preview component by default, which
  # doesn't exist here since nixpkgs' rustc isn't rustup-managed. Point it at the
  # llvm-cov/llvm-profdata that ship with llvmPackages.llvm above instead.
  LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
  LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";

  # I don't know why, I don't want to know why, I shouldn't have to wonder why.
  # But tmpdir doesn't exist unless I do this terribleness. All 3 lines.
  shellHook = ''
    bash -c 'mkdir -p $TMPDIR' &
    bash -c 'sleep 1 && mkdir -p $TMPDIR' &
    mkdir -p $TMPDIR || true
  '';
}
