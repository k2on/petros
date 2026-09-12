# The non-flake entry point: `import ./nix { inherit pkgs; }`.
{ pkgs ? import <nixpkgs> { } }:
import ./lib { inherit pkgs; }
