#!/bin/sh
# Install pilotage.
#
#   curl -fsSL https://raw.githubusercontent.com/lambiengcode/pilotage/main/install.sh | sh
#
# Environment:
#   PILOTAGE_VERSION      version to install (default: latest release)
#   PILOTAGE_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   PILOTAGE_REPO         owner/name to download from
#   PILOTAGE_BASE_URL     override the download base (mirrors, air-gapped, testing)
#
# Flags:
#   --dry-run   resolve the target and URL, download nothing

set -eu

REPO="${PILOTAGE_REPO:-lambiengcode/pilotage}"
INSTALL_DIR="${PILOTAGE_INSTALL_DIR:-$HOME/.local/bin}"
DRY_RUN=0

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'error: unknown option %s\n' "$arg" >&2; exit 1 ;;
    esac
done

say()  { printf '%s\n' "$*"; }
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "$1 is required but not installed"; }

# --- what are we running on? ------------------------------------------------

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os_part="unknown-linux-gnu" ;;
        Darwin) os_part="apple-darwin" ;;
        MINGW*|MSYS*|CYGWIN*)
            err "on Windows, download the .zip from https://github.com/$REPO/releases" ;;
        *) err "unsupported operating system: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch_part="x86_64" ;;
        aarch64|arm64) arch_part="aarch64" ;;
        *) err "unsupported architecture: $arch (build from source with: cargo install --git https://github.com/$REPO)" ;;
    esac

    printf '%s-%s' "$arch_part" "$os_part"
}

# --- which version? ---------------------------------------------------------

latest_version() {
    # Resolve through the redirect on /releases/latest rather than the API, so this
    # works without a token and is not subject to the API rate limit.
    url="https://github.com/$REPO/releases/latest"
    tag="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url" 2>/dev/null | sed 's#.*/tag/##')"
    # An empty tag means the request failed; an unchanged URL means it did not redirect,
    # which is what happens when the repository has no releases yet.
    if [ -z "$tag" ] || [ "$tag" = "$url" ]; then
        err "could not determine the latest version; set PILOTAGE_VERSION"
    fi
    printf '%s' "$tag"
}

# --- go ---------------------------------------------------------------------

need uname
need curl
need tar

TARGET="$(detect_target)"

if [ -n "${PILOTAGE_VERSION:-}" ]; then
    TAG="$PILOTAGE_VERSION"
    case "$TAG" in v*) ;; *) TAG="v$TAG" ;; esac
elif [ "$DRY_RUN" -eq 1 ]; then
    # A dry run exercises target detection and URL construction. Resolving the real
    # version would make it depend on a published release, which is exactly what a
    # pre-release CI check cannot assume.
    TAG="vLATEST"
else
    TAG="$(latest_version)"
fi
VERSION="${TAG#v}"

ARCHIVE="pilotage-${VERSION}-${TARGET}.tar.gz"
BASE="${PILOTAGE_BASE_URL:-https://github.com/$REPO/releases/download/$TAG}"

say "pilotage $VERSION"
say "  target:      $TARGET"
say "  archive:     $ARCHIVE"
say "  install dir: $INSTALL_DIR"

if [ "$DRY_RUN" -eq 1 ]; then
    say ""
    say "dry run: would download $BASE/$ARCHIVE"
    exit 0
fi

TMP="$(mktemp -d)"
# Clean up on any exit path, including an interrupted download.
trap 'rm -rf "$TMP"' EXIT INT TERM

say ""
say "downloading..."
curl -fsSL "$BASE/$ARCHIVE" -o "$TMP/$ARCHIVE" \
    || err "download failed. Check that $TAG has a release asset for $TARGET: https://github.com/$REPO/releases"

# Verify against the published checksums when they are available. A release without
# SHA256SUMS still installs, but says so rather than pretending it verified.
if curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" 2>/dev/null; then
    expected="$(grep " \{1,2\}\(\./\)\?$ARCHIVE\$" "$TMP/SHA256SUMS" | awk '{print $1}' | head -n1)"
    if [ -n "$expected" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            actual="$(sha256sum "$TMP/$ARCHIVE" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
            actual="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{print $1}')"
        else
            actual=""
        fi
        if [ -z "$actual" ]; then
            say "  checksum: skipped (no sha256sum or shasum on this system)"
        elif [ "$actual" = "$expected" ]; then
            say "  checksum: ok"
        else
            err "checksum mismatch for $ARCHIVE
  expected $expected
  actual   $actual
Refusing to install."
        fi
    else
        say "  checksum: not listed for this archive"
    fi
else
    say "  checksum: SHA256SUMS not published for $TAG"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
BIN="$(find "$TMP" -type f -name pilotage -perm -u+x | head -n1)"
[ -n "$BIN" ] || err "the archive did not contain a pilotage binary"

mkdir -p "$INSTALL_DIR"
# install(1) replaces the file atomically, which matters if the running shell has the
# old binary open.
if command -v install >/dev/null 2>&1; then
    install -m 755 "$BIN" "$INSTALL_DIR/pilotage"
else
    cp "$BIN" "$INSTALL_DIR/pilotage" && chmod 755 "$INSTALL_DIR/pilotage"
fi

say ""
say "installed: $INSTALL_DIR/pilotage"

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        say ""
        say "Run 'pilotage' to get started."
        ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. Add it:"
        say ""
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        say ""
        say "then run 'pilotage' to get started."
        ;;
esac
