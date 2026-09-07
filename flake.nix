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

          # Read from ./rust-toolchain.toml rather than repeated here, so the
          # version cannot drift between the devshell and a plain `cargo`
          # outside it — and so bumping it is a one-line change in one file.
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # SQLite's C, compiled for wasm. Unwrapped on purpose: nix's wrapped
          # clang injects this system's glibc headers, which is exactly what
          # breaks a wasm32-unknown-unknown build.
          wasmClang = pkgs.llvmPackages.clang-unwrapped;
          # clang's own builtin headers. `getLib` rather than a literal path
          # because nixpkgs may put them in the `lib` output, and an unwrapped
          # clang cannot find them on its own.
          wasmResourceDir =
            "${pkgs.lib.getLib wasmClang}/lib/clang/"
            + pkgs.lib.versions.major wasmClang.version;

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
              # `just web` uses this only when its version happens to match the
              # wasm-bindgen crate in Cargo.lock, which nixpkgs cannot promise —
              # the schema versions must be identical. Otherwise the recipe
              # fetches the matching one into ./target by itself.
              wasm-bindgen-cli
              llvmPackages.llvm
              bun
              sqlite
              pkg-config
              # For the audio server's transcoding (phase 3).
              ffmpeg
              
              python3
            ]) ++ icedLibs;

            # iced loads these at runtime rather than linking them.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath icedLibs;

            # `just web` reads these. They are explicit paths rather than a bare
            # `clang` because the devshell also exports `CC=gcc`, and gcc cannot
            # target wasm.
            WASM_CC = "${wasmClang}/bin/clang";
            WASM_AR = "${pkgs.llvmPackages.llvm}/bin/llvm-ar";
            WASM_CFLAGS = "-resource-dir ${wasmResourceDir}";

            shellHook = ''
              # NixOS keeps the host's GPU drivers under /run/opengl-driver, and
              # they are linked against the host's libwayland. LD_LIBRARY_PATH
              # beats a library's own RUNPATH, so on a machine tracking a newer
              # channel than this flake's pin, the wayland above shadows the one
              # Mesa was built for. Every Mesa Vulkan driver then fails to load
              # ("undefined symbol: wl_fixes_interface"), wgpu finds no adapter
              # at all, and iced quietly falls back to its software renderer —
              # which draws the right pixels far too slowly to scroll, and
              # leaves stale ones behind where its damage tracking undershoots.
              # Nothing says so out loud, so let the host's own copy win.
              for driver in /run/opengl-driver/lib/libvulkan_*.so; do
                [ -e "$driver" ] || continue
                # Ask the driver itself, with our own path out of the way, so
                # this keeps working when the host moves on again.
                hostWayland=$(LD_LIBRARY_PATH= ldd "$driver" 2>/dev/null \
                  | sed -n 's|.*=> \(.*\)/libwayland-client\.so\.0 .*|\1|p' \
                  | head -1)
                if [ -n "$hostWayland" ]; then
                  export LD_LIBRARY_PATH="$hostWayland:$LD_LIBRARY_PATH"
                  break
                fi
              done

              echo "harken devshell — just test | just lint | just offline | just serve"
            '';
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
