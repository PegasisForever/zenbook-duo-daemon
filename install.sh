#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVICE_NAME="zenbook-duo-daemon"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
PRE_SLEEP_SERVICE_NAME="zenbook-duo-daemon-pre-sleep"
PRE_SLEEP_SERVICE_FILE="/etc/systemd/system/${PRE_SLEEP_SERVICE_NAME}.service"
POST_SLEEP_SERVICE_NAME="zenbook-duo-daemon-post-sleep"
POST_SLEEP_SERVICE_FILE="/etc/systemd/system/${POST_SLEEP_SERVICE_NAME}.service"
INSTALL_DIR="/opt/zenbook-duo-daemon"
BINARY_PATH="${INSTALL_DIR}/zenbook-duo-daemon"
GITHUB_REPO="https://github.com/PegasisForever/zenbook-duo-daemon/releases/latest/download"

check_root() {
    if [ "$EUID" -ne 0 ]; then
        echo "Error: This script must be run as root" >&2
        exit 1
    fi
}

uninstall() {
    check_root
    
    echo "Uninstalling ${SERVICE_NAME}..."
    
    # Stop and disable main service if it exists
    if systemctl is-active --quiet "${SERVICE_NAME}" 2>/dev/null; then
        echo "Stopping service..."
        systemctl stop "${SERVICE_NAME}" || true
    fi
    
    if systemctl is-enabled --quiet "${SERVICE_NAME}" 2>/dev/null; then
        echo "Disabling service..."
        systemctl disable "${SERVICE_NAME}" || true
    fi
    
    # Stop and disable pre-sleep service if it exists
    if systemctl is-active --quiet "${PRE_SLEEP_SERVICE_NAME}" 2>/dev/null; then
        echo "Stopping pre-sleep service..."
        systemctl stop "${PRE_SLEEP_SERVICE_NAME}" || true
    fi
    
    if systemctl is-enabled --quiet "${PRE_SLEEP_SERVICE_NAME}" 2>/dev/null; then
        echo "Disabling pre-sleep service..."
        systemctl disable "${PRE_SLEEP_SERVICE_NAME}" || true
    fi
    
    # Stop and disable post-sleep service if it exists
    if systemctl is-active --quiet "${POST_SLEEP_SERVICE_NAME}" 2>/dev/null; then
        echo "Stopping post-sleep service..."
        systemctl stop "${POST_SLEEP_SERVICE_NAME}" || true
    fi
    
    if systemctl is-enabled --quiet "${POST_SLEEP_SERVICE_NAME}" 2>/dev/null; then
        echo "Disabling post-sleep service..."
        systemctl disable "${POST_SLEEP_SERVICE_NAME}" || true
    fi
    
    # Remove service files
    if [ -f "${SERVICE_FILE}" ]; then
        echo "Removing main service file..."
        rm -f "${SERVICE_FILE}"
    fi
    
    if [ -f "${PRE_SLEEP_SERVICE_FILE}" ]; then
        echo "Removing pre-sleep service file..."
        rm -f "${PRE_SLEEP_SERVICE_FILE}"
    fi
    
    if [ -f "${POST_SLEEP_SERVICE_FILE}" ]; then
        echo "Removing post-sleep service file..."
        rm -f "${POST_SLEEP_SERVICE_FILE}"
    fi
    
    systemctl daemon-reload
    
    # Remove installation directory
    if [ -d "${INSTALL_DIR}" ]; then
        echo "Removing installation directory..."
        rm -rf "${INSTALL_DIR}"
    fi
    
    echo "Uninstallation complete."
}

install() {
    check_root
    
    echo "Installing ${SERVICE_NAME}..."
    
    # Uninstall old version first
    echo "Uninstalling old version..."
    uninstall
    
    # Create installation directory
    echo "Creating installation directory..."
    mkdir -p "${INSTALL_DIR}"
    
    # Download binary
    echo "Downloading binary from ${GITHUB_REPO}/zenbook-duo-daemon..."
    if ! curl -fSL -o "${BINARY_PATH}" "${GITHUB_REPO}/zenbook-duo-daemon"; then
        echo "Error: Failed to download binary" >&2
        exit 1
    fi
    if [ ! -s "${BINARY_PATH}" ]; then
        echo "Error: Downloaded binary is empty" >&2
        rm -f "${BINARY_PATH}"
        exit 1
    fi
    chmod +x "${BINARY_PATH}"
    
    # Download main service file
    echo "Downloading service file from ${GITHUB_REPO}/zenbook-duo-daemon.service..."
    if ! curl -fSL -o "${SERVICE_FILE}" "${GITHUB_REPO}/zenbook-duo-daemon.service"; then
        echo "Error: Failed to download service file" >&2
        rm -f "${BINARY_PATH}"
        exit 1
    fi
    if [ ! -s "${SERVICE_FILE}" ]; then
        echo "Error: Downloaded service file is empty" >&2
        rm -f "${BINARY_PATH}" "${SERVICE_FILE}"
        exit 1
    fi
    
    # Download pre-sleep service file
    echo "Downloading pre-sleep service file from ${GITHUB_REPO}/zenbook-duo-daemon-pre-sleep.service..."
    if ! curl -fSL -o "${PRE_SLEEP_SERVICE_FILE}" "${GITHUB_REPO}/zenbook-duo-daemon-pre-sleep.service"; then
        echo "Error: Failed to download pre-sleep service file" >&2
        rm -f "${BINARY_PATH}" "${SERVICE_FILE}"
        exit 1
    fi
    if [ ! -s "${PRE_SLEEP_SERVICE_FILE}" ]; then
        echo "Error: Downloaded pre-sleep service file is empty" >&2
        rm -f "${BINARY_PATH}" "${SERVICE_FILE}" "${PRE_SLEEP_SERVICE_FILE}"
        exit 1
    fi
    
    # Download post-sleep service file
    echo "Downloading post-sleep service file from ${GITHUB_REPO}/zenbook-duo-daemon-post-sleep.service..."
    if ! curl -fSL -o "${POST_SLEEP_SERVICE_FILE}" "${GITHUB_REPO}/zenbook-duo-daemon-post-sleep.service"; then
        echo "Error: Failed to download post-sleep service file" >&2
        rm -f "${BINARY_PATH}" "${SERVICE_FILE}" "${PRE_SLEEP_SERVICE_FILE}"
        exit 1
    fi
    if [ ! -s "${POST_SLEEP_SERVICE_FILE}" ]; then
        echo "Error: Downloaded post-sleep service file is empty" >&2
        rm -f "${BINARY_PATH}" "${SERVICE_FILE}" "${PRE_SLEEP_SERVICE_FILE}" "${POST_SLEEP_SERVICE_FILE}"
        exit 1
    fi
    
    # Run migrate-config command
    echo "Running config migration..."
    "${BINARY_PATH}" migrate-config || {
        echo "Warning: Config migration failed, continuing anyway..." >&2
    }
    
    # Reload systemd and enable/start services
    echo "Reloading systemd daemon..."
    systemctl daemon-reload
    
    echo "Enabling services..."
    systemctl enable "${SERVICE_NAME}"
    systemctl enable "${PRE_SLEEP_SERVICE_NAME}"
    systemctl enable "${POST_SLEEP_SERVICE_NAME}"
    
    echo "Starting main service..."
    systemctl start "${SERVICE_NAME}"
    
    echo "Installation complete."
    echo "Service status:"
    systemctl status "${SERVICE_NAME}" --no-pager || true
}

local_install() {
    check_root
    
    if [ -z "$1" ]; then
        echo "Error: local-install requires a binary path argument" >&2
        echo "Usage: $0 local-install <binary-path>" >&2
        exit 1
    fi
    
    local binary_path="$1"
    
    local service_file_path="${SCRIPT_DIR}/zenbook-duo-daemon.service"
    local pre_sleep_service_file_path="${SCRIPT_DIR}/zenbook-duo-daemon-pre-sleep.service"
    local post_sleep_service_file_path="${SCRIPT_DIR}/zenbook-duo-daemon-post-sleep.service"
    
    if [ ! -f "${binary_path}" ]; then
        echo "Error: Binary file not found: ${binary_path}" >&2
        exit 1
    fi
    
    if [ ! -f "${service_file_path}" ]; then
        echo "Error: Service file not found: ${service_file_path}" >&2
        exit 1
    fi
    
    if [ ! -f "${pre_sleep_service_file_path}" ]; then
        echo "Error: Pre-sleep service file not found: ${pre_sleep_service_file_path}" >&2
        exit 1
    fi
    
    if [ ! -f "${post_sleep_service_file_path}" ]; then
        echo "Error: Post-sleep service file not found: ${post_sleep_service_file_path}" >&2
        exit 1
    fi
    
    echo "Installing ${SERVICE_NAME} from local files..."
    
    # Uninstall old version first
    echo "Uninstalling old version..."
    uninstall
    
    # Create installation directory
    echo "Creating installation directory..."
    mkdir -p "${INSTALL_DIR}"
    
    # Copy binary
    echo "Copying binary from ${binary_path}..."
    cp "${binary_path}" "${BINARY_PATH}"
    chmod +x "${BINARY_PATH}"
    
    # Copy service files
    echo "Copying service file from ${service_file_path}..."
    cp "${service_file_path}" "${SERVICE_FILE}"
    
    echo "Copying pre-sleep service file from ${pre_sleep_service_file_path}..."
    cp "${pre_sleep_service_file_path}" "${PRE_SLEEP_SERVICE_FILE}"
    
    echo "Copying post-sleep service file from ${post_sleep_service_file_path}..."
    cp "${post_sleep_service_file_path}" "${POST_SLEEP_SERVICE_FILE}"
    
    # Run migrate-config command
    echo "Running config migration..."
    "${BINARY_PATH}" migrate-config || {
        echo "Warning: Config migration failed, continuing anyway..." >&2
    }
    
    # Reload systemd and enable/start services
    echo "Reloading systemd daemon..."
    systemctl daemon-reload
    
    echo "Enabling services..."
    systemctl enable "${SERVICE_NAME}"
    systemctl enable "${PRE_SLEEP_SERVICE_NAME}"
    systemctl enable "${POST_SLEEP_SERVICE_NAME}"
    
    echo "Starting main service..."
    systemctl start "${SERVICE_NAME}"
    
    echo "Installation complete."
    echo "Service status:"
    systemctl status "${SERVICE_NAME}" --no-pager || true
}

# Main command dispatcher
case "${1:-}" in
    uninstall)
        uninstall
        ;;
    install)
        install
        ;;
    local-install)
        local_install "$2"
        ;;
    *)
        echo "Usage: $0 {uninstall|install|local-install <binary-path>}" >&2
        echo "" >&2
        echo "Commands:" >&2
        echo "  uninstall          Remove service files and delete /opt/zenbook-duo-daemon" >&2
        echo "  install            Download and install from GitHub releases" >&2
        echo "  local-install      Install using local binary and service files" >&2
        exit 1
        ;;
esac
