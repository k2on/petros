# The generator, built the way an app builds it — so `nix build .#codegen`
# here is what tells you the vendored hash in `nix/lib/codegen.nix` is still
# right, before an app finds out.
{
  perSystem = { pkgs, petros, ... }: {
    packages.codegen = petros.mkCodegen {
      toolchain = pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;
    };
  };
}
