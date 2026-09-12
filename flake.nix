{
  description = "petros — a general-purpose, offline-first sync engine";

  inputs = {
    # Pinned to a release branch here; the exact revision lives in flake.lock,
    # which is what actually makes the shell reproducible. Run `nix flake update`
    # deliberately, never as a side effect.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    import-tree.url = "github:vic/import-tree";
  };

  # Dendritic: every file under `modules/` is a flake-parts module, and this
  # file names no outputs. An app wants `flakeModules.default`, which puts
  # `petros` — the crate list, the `[patch]`, the code generator — in scope of
  # its every `perSystem`; `lib.mkPetros pkgs` is the same for a flake that is
  # not flake-parts, and `nix/default.nix` for no flake at all.
  outputs = inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (inputs.import-tree ./modules);
}
