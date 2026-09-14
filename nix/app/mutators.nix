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
          log = lib.mkOption {
            type = lib.types.str;
            default = "mutations.txt";
            description = ''
              The recorded surface of this app's mutations, relative to the
              root. `nix flake check` compares every build against it and
              refuses a change the log cannot survive — a verb removed or
              renamed, an argument dropped or retyped. Adding is always
              allowed. `nix run .#log-snapshot` rewrites it, which is how a
              change gets made deliberately rather than by accident.

              On by default, and there is no good reason to turn it off: the
              log is permanent whether or not anyone is watching it.
            '';
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
        # Record what this build declares. The deliberate act — a diff in a
        # review that shows exactly what the log's surface gained.
        log-snapshot.program = script "log-snapshot" {
          runtimeInputs = [ self'.packages.petrosCodegen ];
          text = ''
            ${self'.apps.mutators.program}
            log-compat target/wasm32-unknown-unknown/${cfg.profile}/${cfg.crate}.wasm \
              ${cfg.log} --write
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
