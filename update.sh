#!/usr/bin/env bash
set -euo pipefail

REPO="d3intran/agygram"
INSTALL_PATH="${AGYGRAM_INSTALL_PATH:-$HOME/agygram}"
INSTALL_DIR="$(dirname -- "$INSTALL_PATH")"
mkdir -p -- "$INSTALL_DIR"

TMP_BIN="$(mktemp "${INSTALL_PATH}.tmp.XXXXXX")"
TMP_SHA="$(mktemp "${INSTALL_PATH}.sha256.XXXXXX")"

cleanup() {
    rm -f -- "$TMP_BIN" "$TMP_SHA"
}
trap cleanup EXIT INT TERM

echo "==> Fetching latest release from ${REPO}..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "https://github.com/${REPO}/releases/latest/download/agygram" -o "$TMP_BIN"
    curl -fsSL "https://github.com/${REPO}/releases/latest/download/agygram.sha256" -o "$TMP_SHA"
elif command -v gh >/dev/null 2>&1; then
    gh release download --repo "$REPO" --pattern "agygram" --output "$TMP_BIN" --clobber
    gh release download --repo "$REPO" --pattern "agygram.sha256" --output "$TMP_SHA"
else
    echo "Error: curl or gh is required to download agygram." >&2
    exit 1
fi

echo "==> Verifying SHA-256 checksum..."
EXPECTED_SHA="$(awk 'NR==1 {print $1}' "$TMP_SHA")"
if [ -z "$EXPECTED_SHA" ]; then
    echo "Error: empty SHA-256 checksum file." >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA="$(sha256sum "$TMP_BIN" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA="$(shasum -a 256 "$TMP_BIN" | awk '{print $1}')"
else
    echo "Error: sha256sum or shasum is required to verify download integrity." >&2
    exit 1
fi

if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
    echo "Error: SHA-256 mismatch! expected=$EXPECTED_SHA actual=$ACTUAL_SHA" >&2
    exit 1
fi

chmod +x -- "$TMP_BIN"

echo "==> Verifying downloaded binary..."
"$TMP_BIN" --version

if [ -f "$INSTALL_PATH" ]; then
    BACKUP_PATH="${INSTALL_PATH}.bak-$(date -u +%Y%m%dT%H%M%SZ)"
    echo "==> Backing up current binary to ${BACKUP_PATH}..."
    cp -f -- "$INSTALL_PATH" "$BACKUP_PATH"

    # Keep only the 3 most recent backups.
    while IFS= read -r old_bak; do
        [ -n "$old_bak" ] && rm -f -- "$old_bak"
    done < <(ls -1t "${INSTALL_PATH}.bak-"* 2>/dev/null | tail -n +4 || true)
fi

echo "==> Installing binary to ${INSTALL_PATH}..."
mv -f -- "$TMP_BIN" "$INSTALL_PATH"
chmod +x -- "$INSTALL_PATH"

if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user list-unit-files agygram.service >/dev/null 2>&1 && systemctl --user is-enabled agygram >/dev/null 2>&1; then
        echo "==> Restarting agygram user systemd service..."
        systemctl --user restart agygram
        systemctl --user status agygram --no-pager
        echo "==> Upgrade complete!"
        exit 0
    elif systemctl list-unit-files agygram.service >/dev/null 2>&1 && systemctl is-enabled agygram >/dev/null 2>&1; then
        echo "==> Restarting agygram systemd service..."
        sudo systemctl restart agygram
        systemctl status agygram --no-pager
        echo "==> Upgrade complete!"
        exit 0
    fi
fi

echo "==> Binary installed at ${INSTALL_PATH}!"
echo "    Run '${INSTALL_PATH} init' to configure, or '${INSTALL_PATH} daemon' to start."
