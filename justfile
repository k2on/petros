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
    cargo run -p petros --example offline

# The multiplayer demo. `just serve` in one terminal, `just peer <name>` in others.
serve addr="127.0.0.1:8787":
    cargo run -p petros --features ws --example multiplayer -- --serve --server {{addr}}

peer user addr="127.0.0.1:8787":
    cargo run -p petros --features ws --example multiplayer -- --user {{user}} --server {{addr}}

# The iced peer on the desktop. Same server as `just peer`.
iced user="bob" addr="127.0.0.1:8787":
    cargo run -p petros --features ws --example iced -- --user {{user}} --server {{addr}}

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

    mkdir -p target/wasm-sqlite-stub crates/petros/examples/web/pkg
    printf '!<arch>\n' > target/wasm-sqlite-stub/libsqlite3.a
    CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS='--cfg getrandom_backend="wasm_js"' \
    CC_wasm32_unknown_unknown="$cc" \
    AR_wasm32_unknown_unknown="${WASM_AR:-llvm-ar}" \
    CFLAGS_wasm32_unknown_unknown="$cflags" \
    SQLITE3_LIB_DIR="$PWD/target/wasm-sqlite-stub" SQLITE3_STATIC=1 \
        cargo build -p petros --features ws --example iced \
            --target wasm32-unknown-unknown --release
    "$bindgen" --target web --no-typescript \
        --out-dir crates/petros/examples/web/pkg \
        target/wasm32-unknown-unknown/release/examples/iced.wasm

# Build it and serve it at http://localhost:8080
web: web-build
    @echo "serving on http://localhost:8080"
    cd crates/petros/examples/web && python3 -m http.server 8080

# ---------------------------------------------------------------- the Expo peer
#
# The domain is defined once, in `crates/todo`. `crates/ffi` wraps it for
# foreign callers with uniffi, and everything TypeScript sees is generated from
# that — so there is no second `apply` to keep in step. See docs/decisions.md.

expo-dir := "clients/expo"
# bun hoists a workspace's binaries to the app, not to the library itself.
ubrn := justfile_directory() / "clients/expo/node_modules/.bin/ubrn"
ffi-lib := if os() == "macos" { "libpetros_todo_ffi.dylib" } else { "libpetros_todo_ffi.so" }

# ------------------------------------------------------------- hot mutators
#
# The domain is a wasm module rather than a symbol inside the app's binary, so
# changing a mutation does not mean rebuilding for the device. `just mutators`
# rebuilds it and rewrites the TypeScript file that carries it; Metro is already
# watching that file, and the app swaps the module when it arrives.

# Build the mutator module and hand it to Metro.
mutators:
    cargo build -p todo-wasm --target wasm32-unknown-unknown --profile mutators
    cargo run -q -p emit-mutators

# Where the time goes in one mutation. Ignored by `just test` because it is a
# measurement and it is slow; run it when a number is in question.
latency:
    cargo test -p petros-todo-ffi --release --test latency -- --ignored --nocapture --test-threads=1

# The loop. Leave this running beside `bun start`, then edit crates/todo-wasm.
mutators-watch:
    @echo "watching crates/todo-wasm — save a file and check the phone"
    watchexec --project-origin . --watch crates/todo-wasm --exts rs \
        --on-busy-update=restart -- just mutators

# Install the JS side. Run once, and again after changing a dependency.
expo-install:
    cd {{expo-dir}} && bun install

# Reads the UniFFI metadata straight back out of a host build of the crate, so
# it needs no NDK and no Xcode. That is what makes "did my change to the Rust
# reach the client?" a question you can answer in a couple of seconds — and the
# `tsc` at the end is what turns "the app still calls the old API" into a
# compile error rather than a crash on a device.
#
# `generate bindings` reads the crate name from `cargo metadata` so it runs at
# the workspace root; `generate turbo-module` reads the library's package.json
# so it runs there. Hence the two directories.

# Regenerate the client's TypeScript and C++ from `crates/ffi`.
ffi-bindings: expo-install
    cargo build -p petros-todo-ffi
    {{ubrn}} generate jsi bindings target/debug/{{ffi-lib}} --library --no-format \
        --ts-dir {{expo-dir}}/modules/petros-todo/src/generated \
        --cpp-dir {{expo-dir}}/modules/petros-todo/cpp/generated
    cd {{expo-dir}}/modules/petros-todo && {{ubrn}} generate jsi turbo-module \
        --config ubrn.config.yaml --native-bindings petros_todo_ffi
    cd {{expo-dir}} && ./node_modules/.bin/tsc --noEmit

# Needs the SDK and the NDK, which the default shell deliberately does not
# carry: `nix develop .#android -c just expo-android`. Expo's native projects
# are generated rather than committed, so `prebuild` runs first and `android/`
# never enters the tree.

# Build the Rust for Android, regenerate, and run the app.
expo-android: expo-install
    cd {{expo-dir}}/modules/petros-todo && {{ubrn}} build android \
        --config ubrn.config.yaml --and-generate --release
    cd {{expo-dir}} && bunx expo prebuild --platform android --clean
    cd {{expo-dir}} && bunx expo run:android

# Needs Xcode, so it only runs on a Mac — in CI that is a macOS runner, see
# `.github/workflows/expo-ios.yml`.

# The same for iOS.
expo-ios: expo-install
    cd {{expo-dir}}/modules/petros-todo && {{ubrn}} build ios \
        --config ubrn.config.yaml --and-generate --release
    cd {{expo-dir}} && bunx expo prebuild --platform ios --clean
    cd {{expo-dir}} && bunx expo run:ios

doc:
    cargo doc -p petros --no-deps --all-features --open
