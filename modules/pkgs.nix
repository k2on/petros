# nixpkgs with the Rust overlay, so `rust-bin.fromRustupToolchainFile` reads
# `rust-toolchain.toml` and the devshell's compiler is the one `cargo`
# outside it would pick.
{ inputs, ... }: {
  perSystem = { system, ... }: {
    _module.args.pkgs = import inputs.nixpkgs {
      inherit system;
      overlays = [ (import inputs.rust-overlay) ];
    };
  };
}
