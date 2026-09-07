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
            # Only the Android SDK needs either of these, and only in the
            # `android` shell below. Google ships it under a licence nixpkgs
            # classifies as unfree, and there is no way to accept it per-shell.
            config = {
              allowUnfree = true;
              android_sdk.accept_license = true;
            };
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

          # The Android SDK and NDK, for building `crates/ffi` into the Expo
          # client. Deliberately *not* in the default shell: it is several
          # gigabytes, direnv loads the default shell on every `cd` into this
          # tree, and nothing else here needs it. `nix develop .#android`.
          #
          # This shell is x86_64-linux and x86_64-darwin only in practice.
          # Google ships the NDK as prebuilt binaries and publishes no
          # aarch64-linux host toolchain, so on an ARM Linux box (an Asahi Mac,
          # say) the SDK resolves but its `clang` cannot execute — binfmt hands
          # it to qemu, which then has no x86-64 loader to give it. Build
          # Android on an x86_64 machine or in CI; nothing about the
          # configuration below changes.
          android = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "35" ];
            buildToolsVersions = [ "35.0.0" ];
            includeNDK = true;
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

          # Everything the default shell has, plus the Android SDK and NDK.
          #
          #     nix develop .#android -c just expo-android
          #
          # Separate because it is several gigabytes and direnv loads the
          # default shell every time you `cd` in here. iOS has no equivalent:
          # it needs Xcode, so it is built on a macOS runner by
          # `.github/workflows/ios.yml` rather than anywhere on Linux.
          android = pkgs.mkShell {
            inputsFrom = [ self.devShells.${system}.default ];

            packages = with pkgs; [
              # Turns a `cargo build` into one that knows about the NDK's
              # toolchain, sysroot and target triples, and drops the result
              # where gradle expects to find it.
              cargo-ndk
              jdk17
              android.androidsdk
            ];

            ANDROID_HOME = "${android.androidsdk}/libexec/android-sdk";
            ANDROID_SDK_ROOT = "${android.androidsdk}/libexec/android-sdk";
            ANDROID_NDK_ROOT = "${android.androidsdk}/libexec/android-sdk/ndk-bundle";
            JAVA_HOME = "${pkgs.jdk17}";

            shellHook = ''
              # gradle refuses to use the SDK's own prebuilt aapt2 on NixOS,
              # because it is a dynamically linked binary against an FHS that
              # is not there. The one in the store is patched; point at it.
              export GRADLE_OPTS="-Dorg.gradle.project.android.aapt2FromMavenOverride=$ANDROID_HOME/build-tools/35.0.0/aapt2"
              echo "harken android shell — just expo-android"
            '';
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixpkgs-fmt);
    };
}
