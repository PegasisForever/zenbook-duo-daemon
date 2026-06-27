#!/usr/bin/env bash
# install.sh — Install the Zenbook Duo GNOME Shell extension for the current user.
#
# Usage:
#   ./install.sh          # install (or reinstall)
#   ./install.sh --enable # install AND enable via gnome-extensions CLI

set -euo pipefail

EXTENSION_UUID="zenbook-duo@zenbook-duo-daemon"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_DIR="${SCRIPT_DIR}/${EXTENSION_UUID}"
DEST_DIR="${HOME}/.local/share/gnome-shell/extensions/${EXTENSION_UUID}"

echo "→ Installing extension to ${DEST_DIR}"
mkdir -p "${DEST_DIR}"
cp -r "${SRC_DIR}/." "${DEST_DIR}/"

echo "→ Done."
echo ""
echo "NOTE: If GNOME Shell does not detect a newly added extension UUID"
echo "      immediately, log out and back in once."
echo ""

if [[ "${1:-}" == "--enable" ]]; then
    echo "→ Enabling extension…"
    gnome-extensions enable "${EXTENSION_UUID}" || \
        echo "  (Could not enable automatically — enable it from the Extensions app)"
fi
