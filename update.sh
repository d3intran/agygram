#!/bin/bash
set -euo pipefail

REPO="d3intran/agygram"
INSTALL_PATH="/home/azureuser/agygram"
TMP_BIN="/tmp/agygram-downloaded"

echo "==> Fetching latest release from ${REPO}..."
gh release download --repo "${REPO}" --pattern "agygram" --output "${TMP_BIN}" --clobber

chmod +x "${TMP_BIN}"

echo "==> Testing downloaded binary..."
"${TMP_BIN}" --help > /dev/null

BACKUP_PATH="${INSTALL_PATH}.bak-$(date +%Y%m%dT%H%M%SZ)"
if [ -f "${INSTALL_PATH}" ]; then
    echo "==> Backing up current binary to ${BACKUP_PATH}..."
    cp -f "${INSTALL_PATH}" "${BACKUP_PATH}"
fi

echo "==> Installing new binary to ${INSTALL_PATH}..."
mv -f "${TMP_BIN}" "${INSTALL_PATH}"
chmod +x "${INSTALL_PATH}"

echo "==> Restarting agygram service..."
sudo systemctl restart agygram

echo "==> Service status:"
systemctl status agygram --no-pager
echo "==> Upgrade complete!"
