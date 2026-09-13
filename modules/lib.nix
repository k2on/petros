# The library, and a Petros app's whole flake.
#
#     outputs = inputs: inputs.petros.lib.mkApp inputs ./.;
#
# `mkApp` is flake-parts over every `*.nix` under the app's root — the
# dendritic pattern, `import-tree` over the tree — plus `nix/app` here, which
# is what every app shares: the toolchain, the workspace, the checks, the
# devshell, the module a domain compiles to. Which directories exist is what
# wires the rest: a `server/nix` adds a server, a `mobile/nix` adds a phone,
# and a program with neither is a Rust crate and a `flake.nix` of a few lines.
# The app's `Cargo.toml` pins the engine; `nix/app/workspace.nix` reads it.
#
# `mkPetros pkgs` is the library alone, for a flake that is not flake-parts.
{ inputs, ... }: {
  flake.lib = {
    mkPetros = pkgs: import ../nix/lib { inherit pkgs; };

    mkApp = appInputs: root:
      inputs.flake-parts.lib.mkFlake { inputs = appInputs; } {
        imports = [
          ../nix/app
          "${inputs.files}/flake-module.nix"
          ((inputs.import-tree.matchNot ".*/flake\\.nix") root)
        ];
        _module.args = {
          appRoot = root;
          petrosInputs = inputs;
        };
      };
  };
}
