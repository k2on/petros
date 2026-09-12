# What an app needs from the engine's repository, as nix: the crate list and
# the `[patch]` that makes its lockfile resolvable, the code generator, and the
# wasm module the generator reads.
#
#     petros = import ./nix/lib { inherit pkgs; };
#
# Building the app for a phone is not here. That is `petros-js`, which knows
# about `uniffi` and Expo; this repository knows about neither.
{ pkgs }:
import ./patch.nix { inherit pkgs; } // import ./codegen.nix { inherit pkgs; }
