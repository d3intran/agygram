#!/bin/bash
set -euo pipefail

REPO="d3intran/agygram"
INSTALL_PATH="${AGYGRAM_INSTALL_PATH:-$HOME/agygram}"
TMP_BIN="/tmp/agygram-downloaded-$$"

echo "==> Fetching latest release from ${REPO}..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "https://github.com/${REPO}/releases/latest/download/agygram" -o "${TMP_BIN}"
elif command -v gh >/dev/null 2>&1; then
    gh release download --repo "${REPO}" --pattern "agygram" --output "${TMP_BIN}" --clobber
else
    echo "Error: curl or gh is required to download agygram." >&2
    exit 1
fi

chmod +x "${TMP_BIN}"

echo "==> Verifying downloaded binary..."
"${TMP_BIN}" --version

if [ -f "${INSTALL_PATH}" ]; then
    BACKUP_PATH="${INSTALL_PATH}.bak-$(date +%Y%m%dT%H%M%SZ)"
    echo "==> Backing up current binary to ${BACKUP_PATH}..."
    cp -f "${INSTALL_PATH}" "${BACKUP_PATH}"
fi

echo "==> Installing binary to ${INSTALL_PATH}..."
mv -f "${TMP_BIN}" "${INSTALL_PATH}"
chmod +x "${INSTALL_PATH}"

if systemctl list-unit-files agygram.service >/dev/null 2>&1 && systemctl is-enabled agygram >/dev/null 2>&1; then
    echo "==> Restarting agygram systemd service..."
    sudo systemctl restart agygram
    systemctl status agygram --no-pager
    echo "==> Upgrade complete!"
else
    echo "==> Binary installed at ${INSTALL_PATH}!"
    echo "    Run '${INSTALL_PATH} init' to configure, or '${INSTALL_PATH} daemon' to start."
fi
