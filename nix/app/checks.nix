# fmt, clippy, the suite — as derivations over the narrow tree, so CI and a
# laptop run one expression and a check whose inputs have not moved does not
# run at all — and the same three as `nix run`, against whatever `target/` is
# lying around.
{
  perSystem = { config, pkgs, toolchain, rustPlatform, sources, script, self', ... }:
    let
      # The domain's wasm module is staged first where there is one:
      # `foreign_peer!` does `include_bytes!` of it, so compiling with
      # `--all-features` needs the file to exist.
      stage = if config.petros.mutators == null then "" else ''
        mkdir -p target/wasm32-unknown-unknown/${config.petros.mutators.profile}
        cp ${self'.packages.mutators}/*.wasm target/wasm32-unknown-unknown/${config.petros.mutators.profile}/
      '';
      check = name: { command, tools ? [ ] }: pkgs.stdenv.mkDerivation {
        name = "${config.petros.name}-check-${name}";
        src = sources.engineWorkspace;
        inherit (sources) cargoDeps;
        nativeBuildInputs = [ rustPlatform.cargoSetupHook toolchain pkgs.pkg-config ] ++ tools;
        buildInputs = config.petros.buildInputs;
        buildPhase = ''
          runHook preBuild
          ${stage}
          ${command}
          runHook postBuild
        '';
        installPhase = "mkdir -p $out";
        dontFixup = true;
      };
      run = if config.petros.mutators == null then "" else self'.apps.mutators.program;
    in
    {
      checks = {
        check-fmt = check "fmt" { command = "cargo fmt --all --check"; };
        check-clippy = check "clippy" {
          command = "cargo clippy --workspace --all-features --all-targets --offline -- -D warnings";
        };
        check-tests = check "tests" {
          tools = [ pkgs.cargo-nextest ];
          command = ''
            cargo nextest run --workspace --all-features --offline
            cargo test --workspace --all-features --doc --offline
          '';
        };
      };
      packages = { inherit (config.checks) check-fmt check-clippy check-tests; };

      apps = {
        fmt.program = script "fmt" { text = "cargo fmt --all"; };
        lint.program = script "lint" {
          text = ''
            ${run}
            cargo clippy --workspace --all-features --all-targets -- -D warnings
            cargo fmt --all --check
          '';
        };
        test.program = script "test" {
          runtimeInputs = [ pkgs.cargo-nextest ];
          text = ''
            ${run}
            cargo nextest run --workspace --all-features
            cargo test --workspace --all-features --doc
          '';
        };
      };
    };
}
