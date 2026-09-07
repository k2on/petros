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

# The iced client in a browser.
#
# `libsqlite3-sys` still emits `-lsqlite3` even when it is not the one providing
# SQLite, so it is pointed at an empty archive; the real symbols come from
# `sqlite-wasm-rs`, linked into the same module. `!<arch>` is a valid empty `ar`
# archive, which saves keeping a binary in the tree.
web-build:
    mkdir -p target/wasm-sqlite-stub clients/iced/pkg
    printf '!<arch>\n' > target/wasm-sqlite-stub/libsqlite3.a
    SQLITE3_LIB_DIR=$PWD/target/wasm-sqlite-stub SQLITE3_STATIC=1 \
        cargo build -p exo-iced-demo --target wasm32-unknown-unknown --release
    wasm-bindgen --target web --no-typescript \
        --out-dir clients/iced/pkg \
        target/wasm32-unknown-unknown/release/exo_iced_demo.wasm

# Build it and serve it at http://localhost:8080
web: web-build
    @echo "serving on http://localhost:8080"
    cd clients/iced && python3 -m http.server 8080

doc:
    cargo doc -p exo --no-deps --all-features --open
