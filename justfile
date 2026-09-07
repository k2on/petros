# Exo / harken developer tasks. `nix develop` provides everything these need.

default: fmt lint test

# Run the whole suite. Must stay under 30 seconds.
test:
    cargo nextest run --workspace --all-features
    cargo test --workspace --all-features --doc

# Clippy over everything, warnings are errors.
lint:
    cargo clippy --workspace --all-features --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

# The offline demo: a client with no server in existence.
offline:
    cargo run -p exo --example offline

# The multiplayer demo. `just serve` in one terminal, `just peer <name>` in others.
serve addr="127.0.0.1:8787":
    cargo run -p exo --features ws --example multiplayer -- --serve --server {{addr}}

peer user addr="127.0.0.1:8787":
    cargo run -p exo --features ws --example multiplayer -- --user {{user}} --server {{addr}}

# The iced client on the desktop.
desktop:
    cargo run -p exo-iced-demo

doc:
    cargo doc -p exo --no-deps --all-features --open
