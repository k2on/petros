# The module itself, in a file import-tree does not load on its own: it is
# what `flakeModules.default` hands a consumer, and what `self.nix` imports
# so this flake is its own first consumer.
{
  perSystem = { pkgs, ... }: {
    _module.args.petros = import ../nix/lib { inherit pkgs; };
  };
}
