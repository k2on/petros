# The library, for a consumer that is not flake-parts:
#
#     petros = inputs.petros.lib.mkPetros pkgs;
{
  flake.lib.mkPetros = pkgs: import ../nix/lib { inherit pkgs; };
}
