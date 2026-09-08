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
              # The Expo client. Metro and the Expo CLI are node programs even
              # when bun installs and runs them, so both are here.
              bun
              nodejs_22
              # Rebuilds the mutator module on save. The whole hot-reload loop
              # is this plus Metro, which is already watching for the .ts it
              # writes.
              watchexec
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

          # Everything the default shell has, plus the two tools that turn Rust
          # into an Android library.
          #
          #     nix develop .#android -c just expo-android
          #
          # The SDK and the NDK are *not* here, and that is deliberate. This
          # flake once composed them with `androidenv`; gradle then failed with
          # "The SDK directory is not writable", because the Android Gradle
          # Plugin resolves versions against the SDK directory and installs
          # whatever is missing — and a nix store path is read-only by
          # construction. Pinning every version to match Expo's exactly would
          # postpone that fight rather than win it: Expo moves its `compileSdk`
          # and `ndkVersion` on its own schedule, and nixpkgs moves on another.
          #
          # So the SDK comes from where it comes from for every other React
          # Native project — Android Studio locally, the runner image in CI —
          # and nix pins the part that is actually ours: the Rust toolchain, its
          # Android targets, cargo-ndk and bun. `crates/ffi` cross-compiles
          # identically either way.
          android = pkgs.mkShell {
            inputsFrom = [ self.devShells.${system}.default ];

            packages = with pkgs; [
              # Gives `cargo build` the NDK's toolchain, sysroot and target
              # triples, and drops the result where gradle looks for it.
              cargo-ndk
              jdk17
            ];

            shellHook = ''
              if [ -z "''${ANDROID_HOME:-}" ] && [ -n "''${ANDROID_SDK_ROOT:-}" ]; then
                export ANDROID_HOME="$ANDROID_SDK_ROOT"
              fi
              if [ -z "''${ANDROID_HOME:-}" ]; then
                echo "android shell: no ANDROID_HOME." >&2
                echo "  Install the SDK (Android Studio, or sdkmanager) and export it." >&2
                echo "  Note that Google publishes no aarch64-linux NDK, so an ARM" >&2
                echo "  Linux machine cannot build this at all — use x86_64, or CI." >&2
              else
                # cargo-ndk looks for these in turn; be explicit rather than
                # depending on which one an SDK install happened to set.
                export ANDROID_SDK_ROOT="''${ANDROID_SDK_ROOT:-$ANDROID_HOME}"
                if [ -z "''${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
                  export ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)"
                fi
                echo "harken android shell — just expo-android"
                echo "  sdk: $ANDROID_HOME"
                echo "  ndk: ''${ANDROID_NDK_HOME:-<none found>}"
              fi
            '';
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
