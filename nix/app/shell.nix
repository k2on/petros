# The devshell: the toolchain, and whatever each directory contributes.
{
  perSystem = { config, pkgs, toolchain, ... }: {
    devShells.default = pkgs.mkShell {
      packages = [ toolchain pkgs.cargo-nextest config.files.writer.drv ] ++ config.petros.shell.packages;
      shellHook = config.petros.shell.hook;
    };
  };
}
