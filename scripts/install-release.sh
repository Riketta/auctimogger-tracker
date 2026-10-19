#!/usr/bin/env sh
# One-command deploy/update of auctimogger-tracker from GitHub releases.
#
# Usage (on the server):
#   sudo AUCTIMOGGER_REPO=owner/repo ./install-release.sh [version]
#
#   AUCTIMOGGER_REPO  required: "owner/repo" of the releases source
#   version           optional tag (e.g. v0.1.0); default = latest release
#
# Expects the layout from deploy/auctimogger-tracker.service (binary under
# /opt/auctimogger-tracker) and restarts the service afterwards.
set -eu

REPO="${AUCTIMOGGER_REPO:?set AUCTIMOGGER_REPO=owner/repo}"
VERSION="${1:-}"
TARGET="x86_64-unknown-linux-gnu"
OPT_DIR="/opt/auctimogger-tracker"

if [ -n "$VERSION" ]; then
    TAG="$VERSION"
else
    echo "==> resolving latest release tag"
    TAG="$(curl -fsSL --retry 3 "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    if [ -z "$TAG" ]; then
        echo "error: could not resolve the latest release tag for ${REPO}" >&2
        exit 1
    fi
fi
ARCHIVE="auctimogger-tracker-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "==> downloading ${ARCHIVE}"
curl -fL --retry 3 -o "$TMP_DIR/archive.tar.gz" "${BASE_URL}/${ARCHIVE}"

# Checksum verification against the release's SHA256SUMS, when published:
# a mismatch aborts the install; a missing checksum file is reported as an
# explicit, skipped verification.
if curl -fsSL --retry 3 -o "$TMP_DIR/SHA256SUMS" "${BASE_URL}/SHA256SUMS"; then
    echo "==> verifying archive checksum"
    EXPECTED="$(grep -E "^[0-9a-fA-F]{64}[[:space:]]+[* ]?${ARCHIVE}\$" \
        "$TMP_DIR/SHA256SUMS" | head -n 1 | awk '{print $1}')"
    if [ -z "$EXPECTED" ]; then
        echo "error: ${ARCHIVE} not listed in the release's SHA256SUMS" >&2
        exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL="$(sha256sum "$TMP_DIR/archive.tar.gz" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL="$(shasum -a 256 "$TMP_DIR/archive.tar.gz" | awk '{print $1}')"
    else
        echo "error: need sha256sum or shasum to verify SHA256SUMS" >&2
        exit 1
    fi
    if [ "$ACTUAL" != "$EXPECTED" ]; then
        echo "error: checksum mismatch for ${ARCHIVE} (expected ${EXPECTED}, got ${ACTUAL})" >&2
        exit 1
    fi
    echo "==> checksum ok"
else
    echo "==> note: ${TAG} publishes no SHA256SUMS; checksum verification skipped"
fi

echo "==> unpacking"
mkdir -p "$TMP_DIR/unpacked"
tar -xzf "$TMP_DIR/archive.tar.gz" -C "$TMP_DIR/unpacked"

BIN="$(find "$TMP_DIR/unpacked" -type f -name auctimogger-tracker | head -n 1)"
if [ -z "$BIN" ]; then
    echo "error: archive contains no auctimogger-tracker binary" >&2
    exit 1
fi

echo "==> installing to ${OPT_DIR}"
install -d "$OPT_DIR"
install -m 0755 "$BIN" "${OPT_DIR}/auctimogger-tracker"

if [ -f /etc/systemd/system/auctimogger-tracker.service ]; then
    echo "==> restarting auctimogger-tracker"
    systemctl restart auctimogger-tracker
    systemctl --no-pager --lines=5 status auctimogger-tracker || true
else
    echo "==> service not installed yet; binary placed. See README.md (Deployment) for first-time setup."
fi
