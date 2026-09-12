# The generator that turns an app's wasm module into TypeScript, and the
# module it reads — `scripts/mutators.sh` as derivations.
{ pkgs }:
let
  inherit (pkgs) lib;

  mkRustPlatform = toolchain: pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
in
{
  # `petros-codegen`, built from this repository without the network.
  #
  # A derivation rather than `cargo install --git`, which is what
  # `mutators.sh` falls back to when there is no sibling checkout — and which
  # is the reason an engine build would otherwise reach the network.
  #
  # The engine's own dependency graph is vendored first: an app's lockfile
  # does not mention `petros-codegen` at all, so this brings its own. The
  # hash is this repository's, for this repository's `Cargo.lock`, and moves
  # when that does — nix prints the right one.
  mkCodegen =
    { toolchain
    , src ? ../..
    , depsHash ? "sha256-WcgEW07qNxOpjun5jJnxcsYOp8wwzrF0I2lRXGNUcB8="
    }:
    let
      vendor = pkgs.stdenv.mkDerivation {
        name = "petros-cargo-vendor";
        inherit src;
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
        outputHash = depsHash;
      };
    in
    (mkRustPlatform toolchain).buildRustPackage {
      pname = "petros-codegen";
      version = "0.1.0";
      inherit src;
      cargoDeps = vendor;
      cargoBuildFlags = [ "-p" "petros-codegen" ];
      doCheck = false;
      meta.mainProgram = "petros-codegen";
    };

  # An app's domain compiled to wasm, and the TypeScript generated from it.
  #
  # An app's checks need it and not merely its tests do: `foreign_peer!`
  # does `include_bytes!` of the module, so *compiling* the crate with
  # `--all-features` needs the file to exist.
  mkMutators =
    { name
      # The app's workspace, with the engine patch installed and the vendored
      # dependencies `cargoDeps` beside it, as `cargoSetupHook` expects.
    , src
    , toolchain
    , cargoDeps
    , codegen
      # The crate that is the domain. Its wasm is `<crate>.wasm`.
    , crate
      # The cargo profile it is built under — an app declares one for this.
    , profile ? "mutators"
    }:
    pkgs.stdenv.mkDerivation {
      inherit name src cargoDeps;
      nativeBuildInputs = [
        (mkRustPlatform toolchain).cargoSetupHook
        toolchain
        codegen
      ];
      buildPhase = ''
        runHook preBuild
        cargo build -p ${crate} --no-default-features \
          --target wasm32-unknown-unknown --profile ${profile} --offline
        petros-codegen \
          target/wasm32-unknown-unknown/${profile}/${crate}.wasm \
          mutators.gen.ts
        runHook postBuild
      '';
      installPhase = ''
        runHook preInstall
        mkdir -p $out
        cp target/wasm32-unknown-unknown/${profile}/${crate}.wasm $out/
        cp mutators.gen.ts $out/
        runHook postInstall
      '';
    };
}
