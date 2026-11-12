#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p .build
if [ ! -e .build/turso ]; then
  git clone --quiet --depth 1 --branch v0.8.3 https://github.com/tursodatabase/turso .build/turso
fi
test "$(git -C .build/turso rev-parse HEAD)" = 607bff9fd0e547a25a97b3bf06e83bcc5d99d53c
rustup toolchain install 1.99.1 --profile minimal --target wasm32-wasip1
export CARGO_TARGET_DIR="$PWD/.build/target"
export RUSTFLAGS="-C link-arg=--max-memory=67108864"
cargo +1.99.1 build --manifest-path engine/Cargo.toml --target wasm32-wasip1 -j 2
# Asyncify lets a cold R2 read suspend Rust even during database open and WAL recovery.
if [ ! -x .build/binaryen-version_123/bin/wasm-opt ]; then
  curl --fail --silent --show-error --location https://github.com/WebAssembly/binaryen/releases/download/version_123/binaryen-version_123-x86_64-linux.tar.gz -o .build/binaryen.tar.gz
  tar xzf .build/binaryen.tar.gz -C .build
fi
.build/binaryen-version_123/bin/wasm-opt .build/target/wasm32-wasip1/debug/merchant_engine.wasm --all-features --asyncify --pass-arg=asyncify-imports@vfs.read,vfs.write --strip-debug -o engine/engine.wasm
