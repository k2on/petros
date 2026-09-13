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
    # Writes the files an app would otherwise repeat nix's facts in, and
    # checks the committed copies. Imported by path: `flake = false`.
    files = {
      url = "github:mightyiam/files";
      flake = false;
    };
  };

  # Dendritic: every file under `modules/` is a flake-parts module, and this
  # file names no outputs. An app wants `lib.mkApp`, which is its whole flake
  # (see `modules/app.nix`); `flakeModules.default` puts the library alone in
  # scope of a flake-parts flake, `lib.mkPetros pkgs` is the same for a flake
  # that is not flake-parts, and `nix/default.nix` for no flake at all.
  outputs = inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (inputs.import-tree ./modules);
}
