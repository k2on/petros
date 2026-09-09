# Petros / harken developer tasks. `nix develop` provides everything these need.

default: fmt lint test

# Run the whole suite. Must stay under 30 seconds.
#
# `mutators` first because `crates/ffi/tests/wasm_mutators.rs` runs the real
# module — `include_bytes!` of an artifact that does not exist yet is a build
# error, not a skipped test.
test: mutators
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
    cargo run -p todo --example offline

# The multiplayer demo. `just serve` in one terminal, `just peer <name>` in others.
serve addr="127.0.0.1:8787":
    cargo run -p todo --features ws --example multiplayer -- --serve --server {{addr}}

peer user addr="127.0.0.1:8787":
    cargo run -p todo --features ws --example multiplayer -- --user {{user}} --server {{addr}}

# The iced peer on the desktop. Same server as `just peer`.
iced user="bob" addr="127.0.0.1:8787":
    cargo run -p todo --features ws --example iced -- --user {{user}} --server {{addr}}

# The iced client in a browser.
#
# Two things this has to say out loud.
#
# `CC_wasm32_unknown_unknown` because gcc cannot target wasm at all, and `cc-rs`
# falls back to plain `CC` when no target-specific override is set — so any
# shell that exports `CC` (the nix devshell does) hijacks the wasm build and
# fails deep inside glibc's headers. The devshell sets `WASM_CC` to an
# *unwrapped* clang, since nix's wrapped one injects glibc include paths and
# would fail the same way.
#
# `SQLITE3_LIB_DIR` because `libsqlite3-sys` still emits `-lsqlite3` even when it
# is not the one providing SQLite, so it is pointed at an empty archive; the real
# symbols come from `sqlite-wasm-rs`, linked into the same module. `!<arch>` is a
# valid empty `ar` archive, which saves keeping a binary in the tree.
web-build:
    #!/usr/bin/env bash
    set -euo pipefail

    cc="${WASM_CC:-clang}"
    # clang needs its own builtin headers — stddef.h and friends. A plain clang
    # finds them next to itself; nix's does not, because they live in a separate
    # output, so the devshell passes the path in and this works it out otherwise.
    cflags="${WASM_CFLAGS:-"-resource-dir $("$cc" -print-resource-dir)"}"

    # The generated glue and the wasm module carry a bindgen schema version that
    # must match exactly, so whatever wasm-bindgen happens to be on PATH is not
    # good enough — nixpkgs' is pinned to its own release and ours moves with
    # Cargo.lock. Fetch the matching one into ./target rather than asking anyone
    # to keep a global install in step.
    want="$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;q}' Cargo.lock)"
    have="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)"
    if [ "$have" = "$want" ]; then
        bindgen=wasm-bindgen
    else
        bindgen="$PWD/target/wasm-tools/bin/wasm-bindgen"
        if [ "$("$bindgen" --version 2>/dev/null | awk '{print $2}' || true)" != "$want" ]; then
            echo "wasm-bindgen ${have:-none} on PATH, need $want — fetching it into target/"
            cargo install wasm-bindgen-cli --locked --version "$want" \
                --root "$PWD/target/wasm-tools"
        fi
    fi

    mkdir -p target/wasm-sqlite-stub examples/todo/web/pkg
    printf '!<arch>\n' > target/wasm-sqlite-stub/libsqlite3.a
    CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS='--cfg getrandom_backend="wasm_js"' \
    CC_wasm32_unknown_unknown="$cc" \
    AR_wasm32_unknown_unknown="${WASM_AR:-llvm-ar}" \
    CFLAGS_wasm32_unknown_unknown="$cflags" \
    SQLITE3_LIB_DIR="$PWD/target/wasm-sqlite-stub" SQLITE3_STATIC=1 \
        cargo build -p todo --features ws --example iced \
            --target wasm32-unknown-unknown --release
    "$bindgen" --target web --no-typescript \
        --out-dir examples/todo/web/pkg \
        target/wasm32-unknown-unknown/release/examples/iced.wasm
    # A browser build that carries the guest ABI imports a module called
    # `petros` that only a host supplies, and the page dies on the bare
    # specifier before rendering anything — with a clean compile and a clean
    # deploy. Cheap to check, invisible otherwise.
    if grep -q 'from "petros"' examples/todo/web/pkg/iced.js; then
      echo "web-build: the bundle imports \"petros\" — the guest ABI is in a browser build" >&2
      exit 1
    fi

# Build it and serve it at http://localhost:8080
web: web-build
    @echo "serving on http://localhost:8080"
    cd examples/todo/web && python3 -m http.server 8080

# ------------------------------------------------------------- the wasm module
#
# `examples/todo` is compiled to wasm as well as linked, so the same `apply` can
# be checked both ways. `tests/conformance.rs` is what makes that a claim rather
# than a hope.

# Build the module the conformance test runs, and the TypeScript types that
# come out of it. The `.ts` has no consumer in this repository — an app is what
# consumes it — but generating it here is what keeps petros-codegen honest.
mutators:
    cargo build -p todo --no-default-features \
        --target wasm32-unknown-unknown --profile mutators
    cargo run -q -p petros-codegen -- \
        target/wasm32-unknown-unknown/mutators/todo.wasm \
        examples/todo/mutators.gen.ts
