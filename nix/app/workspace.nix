# The app's cargo workspace, read from its `Cargo.toml`, and the engine at the
# revision that file pins.
#
# `Cargo.toml` is the one place the engine's revision is written. Nix reads it
# back rather than carrying a pin of its own: the crates, the code generator,
# the `[patch]` and the toolchain all come from that revision, fetched here.
# Bumping the engine is editing `Cargo.toml`, and nothing else.
{ lib, flake-parts-lib, inputs, appRoot, petrosInputs, ... }: {
  options.perSystem = flake-parts-lib.mkPerSystemOption {
    options.petros = {
      name = lib.mkOption {
        type = lib.types.str;
        description = "What derivations are named after. Defaults to the domain crate's package name.";
      };
      cargoVendorHash = lib.mkOption {
        type = lib.types.str;
        description = "The hash of the vendored dependencies; nix prints the right one when it changes.";
      };
      engineSrc = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Paths, relative to the root, that a cross-compile of the engine reads besides the crates.";
      };
      buildInputs = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
        description = "Libraries the whole-workspace checks need to compile a crate.";
      };
      shell.packages = lib.mkOption { type = lib.types.listOf lib.types.package; default = [ ]; };
      shell.hook = lib.mkOption { type = lib.types.lines; default = ""; };
    };
  };

  config.perSystem = { config, system, lib, ... }:
    let
      cfg = config.petros;

      manifest = builtins.fromTOML (builtins.readFile (appRoot + "/Cargo.toml"));
      isWorkspace = manifest ? workspace;
      members = if isWorkspace then manifest.workspace.members else [ "." ];
      deps = if isWorkspace then manifest.workspace.dependencies else manifest.dependencies;
      crateName = dir: (builtins.fromTOML (builtins.readFile (appRoot + "/${dir}/Cargo.toml"))).package.name;

      # The engine, at the revision `Cargo.toml` pins.
      petrosSrc = builtins.fetchGit {
        url = deps.petros.git;
        rev = deps.petros.rev;
        shallow = true;
      };
      petros = import "${petrosSrc}/nix" { inherit pkgs; };

      # The app's nixpkgs if it has one, so there is one copy in the closure.
      pkgs = import (inputs.nixpkgs or petrosInputs.nixpkgs) {
        inherit system;
        overlays = [ (import petrosInputs.rust-overlay) ];
      };

      # The engine's toolchain is the app's: one version, every target any
      # client is built for.
      toolchainFile = "${petrosSrc}/rust-toolchain.toml";
      toolchain = pkgs.rust-bin.fromRustupToolchainFile toolchainFile;
      rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };

      # The trees everything is built from. `cleanSource` filters VCS files,
      # not gitignored ones, so the local `.cargo/config.toml` — whose patch
      # points at a working copy — is excluded by hand; the patch pointing at
      # the pinned engine is installed instead. `engineSrc` is the Rust alone,
      # so that a screen edit is not an input to a cross-compile or a check.
      clean = name: keep: lib.cleanSourceWith {
        inherit name;
        src = appRoot;
        filter = path: _: keep (lib.removePrefix (toString appRoot + "/") (toString path));
      };
      wanted = [ "Cargo.toml" "Cargo.lock" ] ++ cfg.engineSrc
        ++ lib.concatMap (m: if m == "." then [ "src" "tests" "benches" "build.rs" ] else [ m ]) members;
      src = clean "${cfg.name}-src"
        (rel: !(builtins.elem (baseNameOf rel) [ ".cargo" "target" "node_modules" "result" ]));
      engineSrc = clean "${cfg.name}-engine-src"
        (rel: lib.any (w: lib.hasPrefix w rel || lib.hasPrefix rel w) wanted);
      patched = name: tree: pkgs.runCommand name { } ''
        cp -r ${tree} $out
        chmod -R u+w $out
        install -Dm444 ${petros.mkCargoPatch petrosSrc} $out/.cargo/config.toml
        if grep -q 'source = "git+${deps.petros.git}' $out/Cargo.lock; then
          echo "Cargo.lock records the engine as a git source: something ran cargo" >&2
          echo "without .cargo/config.toml. Run: git checkout -- Cargo.lock" >&2
          exit 1
        fi
      '';
      sources = {
        workspace = patched "${cfg.name}-workspace" src;
        engineWorkspace = patched "${cfg.name}-engine-workspace" engineSrc;
        # The dependencies, vendored by cargo itself — `importCargoLock` and
        # `fetchCargoVendor` send a User-Agent crates.io answers 403 to. A
        # fixed-output derivation: the network is allowed, the result pinned.
        cargoDeps = pkgs.stdenv.mkDerivation {
          name = "${cfg.name}-cargo-vendor";
          src = sources.workspace;
          nativeBuildInputs = [ toolchain pkgs.cacert pkgs.git ];
          buildPhase = ''
            export CARGO_HOME=$PWD/.cargo-home
            mkdir -p $out
            cargo vendor --locked --versioned-dirs $out > $out/config.toml
            cp Cargo.lock $out/Cargo.lock
          '';
          dontInstall = true;
          dontFixup = true;
          outputHashMode = "recursive";
          outputHashAlgo = "sha256";
          outputHash = cfg.cargoVendorHash;
        };
      };
    in
    {
      petros.name = lib.mkDefault (crateName (builtins.head members));

      _module.args = {
        inherit pkgs toolchain rustPlatform sources petros petrosSrc appRoot;
        rustVersion = (builtins.fromTOML (builtins.readFile toolchainFile)).toolchain.channel;

        # A program for `nix run`, run from the repository root.
        script = name: { runtimeInputs ? [ ], text }: pkgs.writeShellApplication {
          inherit name;
          runtimeInputs = [ toolchain pkgs.git ] ++ runtimeInputs;
          text = ''
            cd "$(git rev-parse --show-toplevel)"
          '' + text;
        };

        # A binary crate of the workspace, as a package.
        crate = pname: attrs: rustPlatform.buildRustPackage ({
          inherit pname;
          version = "0.1.0";
          src = sources.workspace;
          inherit (sources) cargoDeps;
          cargoBuildFlags = [ "-p" pname ];
          doCheck = false;
          meta.mainProgram = pname;
        } // attrs);
      };
    };
}
