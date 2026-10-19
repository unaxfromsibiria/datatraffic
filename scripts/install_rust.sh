#!/usr/bin/env bash
# install_rust.sh — vendor libdatatraffic into a Rust project as a prebuilt .so + API module.
#
# Usage:
#   scripts/install_rust.sh /path/to/consumer/project [branch]      # from this repo checkout
#   curl -sL https://raw.githubusercontent.com/unaxfromsibiria/datatraffic/main/scripts/install_rust.sh \
#        | bash -s -- /path/to/consumer/project                     # knowing only GitHub
#
# What it does:
#   1. Locates the source: this checkout when run from inside it, otherwise a shallow clone of
#      $DATATRAFFIC_REPO_URL (default: the public GitHub repo).
#   2. Builds the release cdylib with cargo if it is not present yet — unless
#      DATATRAFFIC_LIB=/path/to/libdatatraffic.so points at an already-built library, in which
#      case no clone/build of the library is needed (the API module still comes from source).
#   3. Copies into the consumer project:
#        vendor/datatraffic/libdatatraffic.so            shared library (link name)
#        vendor/datatraffic/libdatatraffic.so.1.<arch>   same file under its SONAME (runtime)
#        src/datatraffic.rs                              API module; add `mod datatraffic;`
#        .cargo/config.toml with -L/rpath flags          only if you don't have one yet
#
# The consumer project must be a cargo project (Cargo.toml present).

set -euo pipefail

REPO_URL="${DATATRAFFIC_REPO_URL:-https://github.com/unaxfromsibiria/datatraffic.git}"

usage() { echo "usage: install_rust.sh <consumer-project-dir> [branch]" >&2; exit 1; }
[[ $# -ge 1 ]] || usage
CONSUMER_DIR="$(cd "$1" && pwd)"
BRANCH="${2:-main}"

if [[ ! -f "$CONSUMER_DIR/Cargo.toml" ]]; then
    echo "error: $CONSUMER_DIR is not a cargo project (no Cargo.toml found)" >&2
    exit 1
fi

# Same arch naming as `make build-lib` (x86_64, aarch64, ...).
ARCH="$(uname -m)"
SO_NAME="libdatatraffic.so.1.$ARCH"

# --- 1) Locate the source checkout (needed for the API module) --------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
SRC_DIR=""
CLONE_DIR=""
if [[ -f "$SCRIPT_DIR/../libdatatraffic/Cargo.toml" ]]; then
    SRC_DIR="$SCRIPT_DIR/.."
else
    CLONE_DIR="$(mktemp -d)"
    trap 'rm -rf "$CLONE_DIR"' EXIT
    echo ">> cloning $REPO_URL ($BRANCH) ..."
    git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$CLONE_DIR/repo"
    SRC_DIR="$CLONE_DIR/repo"
fi

# --- 2) Locate or build the shared library -----------------------------------------

if [[ -n "${DATATRAFFIC_LIB:-}" ]]; then
    LIB_SRC="$(cd "$(dirname "$DATATRAFFIC_LIB")" && pwd)/$(basename "$DATATRAFFIC_LIB")"
    if [[ ! -f "$LIB_SRC" ]]; then
        echo "error: DATATRAFFIC_LIB=$LIB_SRC not found" >&2
        exit 1
    fi
else
    LIB_SRC="$SRC_DIR/target/release/libdatatraffic.so"
    if [[ ! -f "$LIB_SRC" ]]; then
        if ! command -v cargo >/dev/null; then
            echo "error: cargo not found and no prebuilt library at $LIB_SRC." >&2
            echo "       Install the Rust toolchain (https://rustup.rs) or set" >&2
            echo "       DATATRAFFIC_LIB=/path/to/libdatatraffic.so to reuse a built one." >&2
            exit 1
        fi
        echo ">> building libdatatraffic (cargo build --release) ..."
        # Same SONAME convention as `make build-lib` / setup.py.
        ( cd "$SRC_DIR" && RUSTFLAGS="-C link-args=-Wl,-soname,$SO_NAME" cargo build --release -p libdatatraffic )
    fi
fi

# --- 3) Copy artifacts into the consumer project ------------------------------------

VENDOR_DIR="$CONSUMER_DIR/vendor/datatraffic"
mkdir -p "$VENDOR_DIR"
cp "$LIB_SRC" "$VENDOR_DIR/libdatatraffic.so"
ln -sf "libdatatraffic.so" "$VENDOR_DIR/$SO_NAME"

API_DST="$CONSUMER_DIR/src/datatraffic.rs"
if [[ -f "$API_DST" ]] && ! cmp -s "$SRC_DIR/rust_api/datatraffic.rs" "$API_DST"; then
    mv "$API_DST" "$API_DST.orig"
    echo ">> existing $API_DST differs from upstream; kept as $(basename "$API_DST").orig"
fi
mkdir -p "$CONSUMER_DIR/src"
cp "$SRC_DIR/rust_api/datatraffic.rs" "$API_DST"

# --- 4) Link configuration -----------------------------------------------------------

CONFIG_FILE="$CONSUMER_DIR/.cargo/config.toml"
if [[ ! -f "$CONFIG_FILE" ]]; then
    mkdir -p "$CONSUMER_DIR/.cargo"
    cat > "$CONFIG_FILE" <<EOF
# Added by datatraffic install_rust.sh — link flags for the vendored libdatatraffic.so.
[build]
rustflags = [
    "-L", "native=$VENDOR_DIR",
    "-C", "link-args=-Wl,-rpath,$VENDOR_DIR",
]
EOF
    LINK_NOTE="wrote $CONFIG_FILE with the -L/rpath flags"
else
    LINK_NOTE="kept your existing .cargo/config.toml; add these flags manually if needed:
   RUSTFLAGS=\"-L native=$VENDOR_DIR -C link-args=-Wl,-rpath,$VENDOR_DIR\" cargo build"
fi

# --- Summary -------------------------------------------------------------------------

cat <<EOF

Installed libdatatraffic into $CONSUMER_DIR:
  vendor/datatraffic/libdatatraffic.so        (from $LIB_SRC)
  src/datatraffic.rs                          (API module)
  $LINK_NOTE

Next steps:
  1. Add \`mod datatraffic;\` to your crate root (src/main.rs or src/lib.rs).
  2. Use the safe interface, e.g.:

       use datatraffic::{Client, Config};

       let cfg = Config {
           redis_url: "redis://localhost:6379".into(),
           key_hex: std::env::var("CRYPTO_KEY")?,
           watch_projects: vec!["demo".into()],
           ..Default::default()
       };
       let mut client = Client::new(&cfg)?;
       client.send("clip.wav", "encoded audio", &payload)?;
       while let Some(file) = client.recv(1000) { /* file.name / note / data */ }

  The rpath is baked into built binaries, so no LD_LIBRARY_PATH is needed at runtime.
EOF
