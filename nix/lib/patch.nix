# The engine's crates, and the `[patch]` that points an app's lockfile at them.
{ pkgs }:
let
  inherit (pkgs) lib;
in
rec {
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
}
