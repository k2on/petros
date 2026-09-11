# Building a Petros app's native half for Android.
#
# This is the engine's business rather than any one app's. Every Petros app
# that reaches a phone needs the same SDK pinned the same way, the same `ubrn`
# command, the same list of engine crates to patch — and the same answer to why
# a one-line change to a mutation recompiles a hundred crates it did not touch.
# harken had all of it inline, and the second app would have copied it.
#
# From an app's flake, where `petros` is this repository as a `flake = false`
# input:
#
#     android = import "${petros}/nix/android.nix" { inherit pkgs nixpkgs; };
#
#     inherit (android.mkEngine {
#       name = "harken";
#       src = engineWorkspace;          # the app's tree, narrowed
#       inherit toolchain nodeModules;
#       sdk = android.mkSdk { };
#       ubrn = android.mkUbrn { inherit toolchain nodeModules; };
#       petrosSrc = petros;
#       vendor = cargoDeps;
#       moduleDir = "clients/expo/modules/harken-native";
#       appCrates = [ "crates/harken" "crates/server" "clients/iced" ];
#     }) engine;
{ pkgs, nixpkgs }:

let
  inherit (pkgs) lib;

  # The version every part of an Android build has to agree on. `cargo ndk` and
  # the Android Gradle Plugin looking at two different NDKs is a whole
  # afternoon, so it is named once.
  defaultNdk = "27.1.12297006";

  # cc-rs looks for a compiler under the target triple with its dashes turned
  # into underscores, and prefers that over the bare `CC`. This is the machine
  # doing the building, so the same expression is right on an ARM laptop and an
  # x86_64 runner.
  hostTriple = builtins.replaceStrings [ "-" ] [ "_" ] pkgs.stdenv.buildPlatform.config;

  # The engine's crates, as they are named in a dependency table. An app should
  # not have to know this list to patch the engine — it changes when the engine
  # changes, which is here.
  engineCrates = [
    "petros"
    "petros-wasm-host"
    "petros-schema"
    "petros-sql"
    "petros-testkit"
    "petros-wasm-guest"
    "petros-axum"
  ];
in
rec {
  inherit engineCrates defaultNdk;

  # The `[patch]` that points a lockfile's engine at a tree on disk.
  #
  # An app's `Cargo.lock` is written with this patch in place — the engine
  # appears in it as a *path* with no revision — so cargo cannot resolve the
  # lock without it, and neither can `cargo vendor`.
  mkCargoPatch = petrosSrc: pkgs.writeText "petros-patch.toml" (''
    [patch."https://github.com/k2on/petros"]
  '' + lib.concatMapStrings
    (c: ''${c} = { path = "${petrosSrc}/crates/${c}" }
  '')
    engineCrates);

  # The Android SDK, pinned.
  #
  # Everything Google publishes here is `linux-x86_64` and nothing else, so on
  # an ARM machine these run under qemu — which works, including inside a build
  # sandbox, provided `binfmt_misc` is registered.
  #
  # Every component a build touches has to be listed. The Android Gradle Plugin
  # resolves versions against the SDK directory and *installs* what is missing,
  # and what it installs is a raw Google binary that NixOS cannot run; nixpkgs'
  # copies are patched and do. So the rule is: anything Google's build
  # downloads for itself will not run, anything nixpkgs packaged will, and every
  # such component must be pinned and pointed at.
  mkSdk =
    { ndkVersion ? defaultNdk
    , platformVersions ? [ "36" ]
    , buildToolsVersions ? [ "35.0.0" "36.0.0" ]
      # A turbo module has an `externalNativeBuild`, so gradle wants CMake as
      # well as the NDK.
    , cmakeVersions ? [ "3.22.1" ]
    }:
    let
      x86 = import nixpkgs {
        system = "x86_64-linux";
        config = {
          allowUnfree = true;
          android_sdk.accept_license = true;
        };

        # No 32-bit set. `build-tools.nix` adds i686 glibc, zlib and ncurses5
        # whenever the host platform is x86_64 — and this instance always is,
        # because everything Google ships for Android is. On an aarch64 machine
        # that asks nix to build ncurses for a third architecture:
        #
        #   Required system: 'i686-linux'   Current system: 'aarch64-linux'
        #
        # which binfmt is not set up for and which nothing here would run. The
        # only 32-bit files in build-tools 35 and 36 are RenderScript libraries
        # for *Android* targets — `armeabi-v7a` and `x86` — nested several
        # directories below where `autoPatchelf --no-recurse` looks. Verified by
        # composing both versions with this overlay: they build.
        #
        # `final` rather than `prev`, which is not a style question. `prev` is a
        # separate fixpoint, so pointing at it gives a second x86_64 package set
        # whose derivations hash differently from the ordinary ones — and
        # nothing in it substitutes. It removed the i686 build and replaced it
        # with glibc, zlib and ncurses compiled from source on every machine.
        # With `final`, `pkgsi686Linux.glibc` is the same derivation as
        # `glibc`, which the cache already has.
        overlays = [ (final: prev: { pkgsi686Linux = final; }) ];
      };
    in
    (x86.androidenv.composeAndroidPackages {
      ndkVersions = [ ndkVersion ];
      inherit platformVersions buildToolsVersions cmakeVersions;
      includeNDK = true;
      includeEmulator = false;
      includeSystemImages = false;
    }).androidsdk;

  # Gradle at the version a generated project's wrapper asks for. The wrapper
  # would download it, which a builder cannot do.
  mkGradle =
    { version
    , hash
    , jdk ? pkgs.jdk17
    }:
    pkgs.stdenv.mkDerivation {
      pname = "gradle";
      inherit version;
      src = pkgs.fetchzip {
        url = "https://services.gradle.org/distributions/gradle-${version}-bin.zip";
        inherit hash;
      };
      nativeBuildInputs = [ pkgs.makeWrapper ];
      installPhase = ''
        mkdir -p $out
        cp -r . $out/gradle
        makeWrapper $out/gradle/bin/gradle $out/bin/gradle --set JAVA_HOME ${jdk}
      '';
    };

  # The `ubrn` command, built once.
  #
  # `node_modules/.bin/ubrn` is a shim that runs `cargo run` against a crate
  # inside `node_modules`, so the first call in a fresh tree compiles a CLI from
  # source — and in a builder every call is the first call. Built here it is a
  # binary that answers in milliseconds, and it changes only when the lockfile
  # that pins it does.
  mkUbrn =
    { toolchain
    , nodeModules
      # A lockfile for the generator. It does not ship one — the npm package is
      # the built CLI plus its Rust sources, and `cargo` is expected to resolve
      # on the machine that runs it. So the app commits one and passes it here,
      # which is what makes this build pinned rather than merely offline.
    , lockFile
      # That lockfile's dependencies, vendored. Both move together, and with
      # the app's `bun.lock`, because that decides which generator is in
      # `node_modules`; nix prints the right hash when it changes.
    , depsHash
    }:
    let
      ubrnSrc = "${nodeModules}/uniffi-bindgen-react-native";
      vendor = pkgs.stdenv.mkDerivation {
        name = "ubrn-cargo-vendor";
        src = ubrnSrc;
        nativeBuildInputs = [ toolchain pkgs.cacert pkgs.git ];
        buildPhase = ''
          export CARGO_HOME=$PWD/.cargo-home
          cp ${lockFile} Cargo.lock
          mkdir -p $out
          cargo vendor --locked --versioned-dirs $out > $out/config.toml
        '';
        dontInstall = true;
        dontFixup = true;
        outputHashMode = "recursive";
        outputHashAlgo = "sha256";
        outputHash = depsHash;
      };
    in
    pkgs.stdenv.mkDerivation {
      name = "ubrn";
      src = ubrnSrc;
      nativeBuildInputs = [ toolchain pkgs.pkg-config ];
      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        export CARGO_HOME=$TMPDIR/cargo
        cp ${lockFile} Cargo.lock
        chmod u+w Cargo.lock
        mkdir -p .cargo
        cat > .cargo/config.toml <<'VENDOR'
        [source.crates-io]
        replace-with = "vendored-sources"

        [source.vendored-sources]
        directory = "${vendor}"
        VENDOR
        cargo build --release --offline \
          --manifest-path crates/ubrn_cli/Cargo.toml
        runHook postBuild
      '';
      installPhase = ''
        runHook preInstall
        install -Dm755 target/release/uniffi-bindgen-react-native $out/bin/ubrn
        runHook postInstall
      '';
    };

  # A source tree with every Rust file emptied.
  #
  # Manifests, lockfiles and build scripts survive; `lib.rs` becomes nothing.
  # Cargo then resolves the same dependency graph, compiles all of it, and
  # compiles none of the code that changes — which is the whole trick behind
  # `mkEngine`'s first layer.
  stubSources =
    { name
    , src
      # Directories, relative to `src`, whose crates are to be emptied.
    , dirs
    }:
    pkgs.runCommand name { } ''
      cp -r ${src} $out
      chmod -R u+w $out
      for d in ${lib.escapeShellArgs dirs}; do
        [ -d "$out/$d" ] || continue
        # `build.rs` decides what a dependency compiles into, so it stays.
        find "$out/$d" -name '*.rs' -not -name 'build.rs' -print0 \
          | xargs -0 -r -I{} sh -c ': > "{}"'
      done
    '';

  # The engine cross-compiled for Android, in two layers.
  #
  # nix caches a derivation's output whole and cargo starts every derivation
  # with an empty `target/`, so one derivation means every build compiles
  # `ciborium`, `libm`, `uuid`, `libsqlite3-sys` and a hundred others again —
  # crates whose versions are fixed in a lockfile that did not change.
  #
  # So: `thirdParty` compiles the dependency graph from a tree where both the
  # app's crates *and the engine's* are stubs, and `engine` starts from that
  # `target/` with the real sources restored. Changing a mutation recompiles
  # the app's crates; bumping the engine recompiles the engine's; neither
  # touches the hundred.
  #
  # Two things make the reuse actually happen, and both are easy to get wrong:
  #
  #   - the dependencies must live at a path that is identical in both builds,
  #     so `vendor` is required rather than letting cargo fetch into a
  #     per-build `CARGO_HOME`. Measured: without it, six of twelve crates
  #     recompiled anyway.
  #   - the `target/` must be carried with `cp -a`. `cp -r` stamps every file
  #     with the current time and destroys the ordering cargo's fingerprints
  #     are built on. Measured: with `cp -a`, a warm build compiles nothing and
  #     finishes in 0.01s; with `cp -r`, it recompiles the proc-macro chain.
  mkEngine =
    { name
    , src
    , toolchain
    , sdk
    , ubrn
    , nodeModules
    , petrosSrc
      # A vendored crate directory, as `cargo vendor` writes it, with the
      # `config.toml` it prints beside it.
    , vendor
      # Where the turbo module lives, relative to the source root.
    , moduleDir
      # The app's own crates, to stub for the first layer.
    , appCrates
    , ndkVersion ? defaultNdk
      # Run before the engine build, in the source root — an app's module is
      # generated from its own domain and may need building first.
    , preEngine ? ""
      # Run at the end of the engine's install, in the source root, for
      # whatever else an app generated on the way.
    , extraInstall ? ""
    }:
    let
      stubbedPetros = stubSources {
        name = "petros-stubbed";
        src = petrosSrc;
        dirs = [ "crates" ];
      };

      common = {
        nativeBuildInputs = [
          toolchain
          sdk
          pkgs.cargo-ndk
          pkgs.git
          pkgs.nodejs
          pkgs.python3
          pkgs.which
        ];

        # No `__noChroot`. Both layers build in a real sandbox: the crates are
        # vendored, the generator is a derivation rather than a
        # `cargo install --git`, and `ubrn` is pinned by the app's lockfile.
        #
        # That is not only a purity argument. A sandboxed build runs at `/build`
        # on every machine, and a `__noChroot` one runs at
        # `/nix/var/nix/builds/nix-build-…-<pid>-<random>`. Anything that
        # records an absolute path — gradle's task history, ninja's `.cxx` —
        # cannot be carried from one build to the next without it.
        #
        # An app that still needs the network can put it back with
        # `overrideAttrs`, and should expect to say why.

        ANDROID_HOME = "${sdk}/libexec/android-sdk";
        ANDROID_SDK_ROOT = "${sdk}/libexec/android-sdk";
        ANDROID_NDK_HOME = "${sdk}/libexec/android-sdk/ndk/${ndkVersion}";

        # `cargo-ndk` sets `CC` for its child, and cc-rs consults it for *host*
        # artifacts too — which have no NDK sysroot to be built against. A
        # target-qualified variable wins over the bare one.
        #
        # The triple is the *build* machine's. It read `aarch64` here, which is
        # one development box, and set nothing at all on an x86_64 runner —
        # where the only reason it built is that `__noChroot` let the NDK's
        # clang reach the host's `/usr/include`. Under a real sandbox that
        # fails on `stdio.h`, which is how this was found.
        "CC_${hostTriple}" = "gcc";
        "AR_${hostTriple}" = "ar";
      };

      # The cargo configuration both layers share: the vendored dependencies,
      # at one path, plus the patch that makes the lockfile resolvable.
      #
      # Written here rather than taken from the vendor directory. `cargo vendor`
      # prints this to stdout and cleans its output directory as it goes, so a
      # `cargo vendor $out > $out/config.toml` leaves nothing behind — the file
      # is unlinked while the redirect still holds it open. Depending on a
      # layout that a tool actively tidies is not worth the two lines saved.
      cargoConfig = engineSrc: ''
        mkdir -p .cargo
        cat > .cargo/config.toml <<'VENDOR'
        [source.crates-io]
        replace-with = "vendored-sources"

        [source.vendored-sources]
        directory = "${vendor}"
        VENDOR
        cat ${mkCargoPatch engineSrc} >> .cargo/config.toml
      '';

      build = ''
        cd ${moduleDir}
        ${ubrn}/bin/ubrn build android --config ubrn.config.yaml --release
        cd -
      '';

      thirdParty = pkgs.stdenv.mkDerivation (common // {
        name = "${name}-android-deps";
        src = stubSources {
          name = "${name}-stubbed";
          inherit src;
          dirs = appCrates;
        };
        buildPhase = ''
          runHook preBuild
          export HOME=$TMPDIR
          export CARGO_HOME=$TMPDIR/cargo
          ${cargoConfig stubbedPetros}
          ${build}
          runHook postBuild
        '';
        # Only the compiled dependencies are wanted. What the stubs produced is
        # thrown away with the rest of the build directory.
        installPhase = ''
          runHook preInstall
          cp -a target $out
          runHook postInstall
        '';
      });
      engine = pkgs.stdenv.mkDerivation (common // {
        name = "${name}-android-engine";
        inherit src;
        buildPhase = ''
          runHook preBuild
          export HOME=$TMPDIR
          export CARGO_HOME=$TMPDIR/cargo
          ${cargoConfig petrosSrc}

          # `cp -a`, not `cp -r`. See the note above: the mtimes are the
          # fingerprints.
          echo "--- the dependency graph, already compiled"
          cp -a ${thirdParty} target
          chmod -R u+w target

          ${preEngine}

          echo "--- the engine, cross-compiled"
          cd ${moduleDir}
          ${ubrn}/bin/ubrn build android \
            --config ubrn.config.yaml --and-generate --release
          cd -
          runHook postBuild
        '';

        # The whole module, not a selection from it. `--and-generate` writes the
        # module's `build.gradle` and `CMakeLists.txt`, its manifest, its
        # `cpp-adapter.cpp`, its `index.tsx` and its podspec as well as the
        # libraries — and a module directory missing its `build.gradle` fails
        # in Expo's autolinking, during *settings* evaluation, as nothing more
        # specific than `command 'node' finished with non-zero exit value 1`.
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -r ${moduleDir} $out/module
          ${extraInstall}
          runHook postInstall
        '';
      });
    in
    # The engine, with its layers hanging off it. An app's `packages` should be
    # derivations all the way down — `nix flake check` says so — and the first
    # layer is worth naming for `nix build .#androidDeps` when you want to know
    # whether it is the dependencies or your own code that is slow.
    engine // { inherit thirdParty stubbedPetros; };
}
