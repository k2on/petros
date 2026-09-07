{
  description = "harken — a self-hosted, local-first music system";

  inputs = {
    # Pinned to a release branch here; the exact revision lives in flake.lock,
    # which is what actually makes the shell reproducible. Run `nix flake update`
    # deliberately, never as a side effect.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          # The whole toolchain comes from the overlay, so every machine gets
          # the same rustc down to the patch version.
          toolchain = pkgs.rust-bin.stable."1.85.0".default.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          };

          # Runtime libraries the iced client will dlopen (phase 5). Wired up
          # now so the shell does not need revisiting when that lands.
          icedLibs = pkgs.lib.optionals pkgs.stdenv.isLinux (with pkgs; [
            wayland
            libxkbcommon
            libGL
            vulkan-loader
            fontconfig
          ]);
        in
        {
          default = pkgs.mkShell {
            packages = [ toolchain ] ++ (with pkgs; [
              cargo-nextest
              just
              bun
              sqlite
              pkg-config
              # For the audio server's transcoding (phase 3).
              ffmpeg
            ]) ++ icedLibs;

            # iced loads these at runtime rather than linking them.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath icedLibs;

            shellHook = ''
              echo "harken devshell — just test | just lint | just offline | just serve"
            '';
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
