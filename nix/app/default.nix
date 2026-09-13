# What every Petros app's flake is made of. `lib.mkApp` imports this beside
# the app's own `*/nix/*.nix`, so an app writes only what is its own: which
# crate is the domain, which directories are clients, its hashes.
{
  imports = [
    ./systems.nix
    ./workspace.nix
    ./checks.nix
    ./mutators.nix
    ./shell.nix
  ];
}
