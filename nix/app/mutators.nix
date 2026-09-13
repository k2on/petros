# The domain compiled to wasm, and the TypeScript generated from it — for an
# app whose `apply` reaches a client as a module rather than a symbol.
#
# `nix build .#mutators` is the derivation; `nix run .#mutators` is the same
# two steps into the working tree, which is what Metro watches and what a
# local `cargo test` reads; `nix run .#mutators-watch` is that on every save.
{ lib, flake-parts-lib, ... }: {
  options.perSystem = flake-parts-lib.mkPerSystemOption {
    options.petros.mutators = lib.mkOption {
      default = null;
      description = "The crate whose mutations are the module, or null for an app with no module.";
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          crate = lib.mkOption { type = lib.types.str; };
          profile = lib.mkOption { type = lib.types.str; default = "mutators"; };
          ts = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Where `nix run .#mutators` writes the generated TypeScript, relative to the root.";
          };
        };
      });
    };
  };

  config.perSystem = { config, pkgs, lib, toolchain, petros, petrosSrc, sources, script, self', ... }:
    let cfg = config.petros.mutators; in
    lib.mkIf (cfg != null) {
      packages = {
        petrosCodegen = petros.mkCodegen { inherit toolchain; src = petrosSrc; };
        mutators = petros.mkMutators {
          name = "${config.petros.name}-mutators";
          inherit toolchain;
          inherit (sources) cargoDeps;
          inherit (cfg) crate profile;
          src = sources.engineWorkspace;
          codegen = self'.packages.petrosCodegen;
        };
      };

      apps = {
        mutators.program = script "mutators" {
          runtimeInputs = [ self'.packages.petrosCodegen ];
          text = ''
            # `--no-default-features` keeps the engine, SQLite and serde_json
            # out of the module. It is a flag on the build, not a property
            # of a package.
            cargo build -p ${cfg.crate} --no-default-features \
              --target wasm32-unknown-unknown --profile ${cfg.profile}
            ${lib.optionalString (cfg.ts != null) ''
              petros-codegen target/wasm32-unknown-unknown/${cfg.profile}/${cfg.crate}.wasm ${cfg.ts}
            ''}
          '';
        };
        mutators-watch.program = script "mutators-watch" {
          runtimeInputs = [ pkgs.watchexec ];
          text = ''
            echo "watching the domain — save a file and check the client"
            watchexec --project-origin . --exts rs --on-busy-update=restart -- \
              ${self'.apps.mutators.program}
          '';
        };
      };
    };
}
