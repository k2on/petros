# The flake-parts module an app imports.
#
#     imports = [ inputs.petros.flakeModules.default ];
#     perSystem = { petros, ... }: { … petros.mkCargoPatch inputs.petros … };
#
# A path rather than an attribute set: the module system deduplicates by
# key and a path is its own, so an app that imports this directly and again
# through `petros-js` gets it once.
{
  flake.flakeModules.default = ./_petros.nix;
}
