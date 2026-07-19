#!/bin/bash

# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail
# set -x # Temporarily disabled to keep logs clean

# --- 1. SET VARIABLES FROM COMMAND LINE ARGUMENTS ---
# Usage: ./build_osx_universal_pkg.sh <VERSION> <SUFFIX> <CERT_NAME> [CARGO_FLAGS...]

INPUT_VERSION=$1       # e.g., v1.2.0
NAME_SUFFIX=$2         # e.g., "normal" or "private"
INSTALLER_CERT_NAME=$3 # e.g., "Developer ID Installer: Your Name (TEAMID)"
shift 3                # Consume the first three arguments

# Derive the Application certificate name from the Installer one
APP_CERT_NAME=$(echo "${INSTALLER_CERT_NAME}" | sed 's/Installer/Application/')
if [ "$APP_CERT_NAME" == "$INSTALLER_CERT_NAME" ]; then
    echo "::error:: Could not derive Application cert name from Installer cert name: ${INSTALLER_CERT_NAME}"
    echo "::error:: This script expects to be passed the 'Developer ID Installer' certificate."
    exit 1
fi

# Fixed Application Variables
APP_NAME="superseedr"
BINARY_NAME="superseedr"
HANDLER_APP_NAME="superseedr"
PKG_IDENTIFIER="com.github.jagalite.superseedr" 
ICON_FILE_PATH="assets/app_icon.icns"
ICON_FILE_NAME="appicon.icns" 

# --- Safety Check: Icon ---
if [ ! -f "$ICON_FILE_PATH" ]; then
    echo "::error:: Icon file not found at ${ICON_FILE_PATH}"
    exit 1
fi
if [ ! -f "scripts/macos_handler.m" ] || [ ! -f "scripts/macos_handler_info.plist" ]; then
    echo "::error:: One or more macOS handler sources are missing."
    exit 1
fi
if [ ! -x "scripts/macos_pkg_scripts/postinstall" ]; then
    echo "::error:: macOS package postinstall script is missing or not executable."
    exit 1
fi

# Determine Version/Identifier
if [ -z "$INPUT_VERSION" ]; then
    VERSION=$(git rev-parse --short HEAD)
else
    # Strip the 'v' prefix
    VERSION=$(echo "$INPUT_VERSION" | sed 's/^v//')
fi

# Apple bundle versions accept numeric dot-separated components. Release tags
# may include a suffix, while local builds may use a Git hash.
BUNDLE_VERSION=$(echo "$VERSION" | sed -E 's/[^0-9.].*$//; s/\.+$//')
if [ -z "$BUNDLE_VERSION" ]; then
  BUNDLE_VERSION="0.0.0"
fi

# Paths
TUI_BINARY_SOURCE_ARM64="target/aarch64-apple-darwin/release/${BINARY_NAME}"
TUI_BINARY_SOURCE_X86_64="target/x86_64-apple-darwin/release/${BINARY_NAME}"

HANDLER_STAGING_DIR="target/handler_staging_${NAME_SUFFIX}"
HANDLER_APP_PATH="${HANDLER_STAGING_DIR}/${HANDLER_APP_NAME}.app"
HANDLER_SOURCE="scripts/macos_handler.m"
HANDLER_PLIST_SOURCE="scripts/macos_handler_info.plist"
HANDLER_EXECUTABLE="${HANDLER_APP_PATH}/Contents/MacOS/superseedr-handler"
PKG_SCRIPTS_PATH="scripts/macos_pkg_scripts"

UNIVERSAL_STAGING_DIR="target/universal_staging_${NAME_SUFFIX}"
UNIVERSAL_BINARY_PATH="${UNIVERSAL_STAGING_DIR}/${BINARY_NAME}"

if [ "$NAME_SUFFIX" == "private" ]; then
  PKG_NAME="${APP_NAME}-${VERSION}-private-universal-macos.pkg"
else
  PKG_NAME="${APP_NAME}-${VERSION}-universal-macos.pkg"
fi

PKG_OUTPUT_DIR="target/release"
UNSIGNED_PKG_OUTPUT_PATH="${PKG_OUTPUT_DIR}/${APP_NAME}-unsigned.pkg"
SIGNED_PKG_OUTPUT_PATH="${PKG_OUTPUT_DIR}/${PKG_NAME}"
PKG_STAGING_ROOT="target/pkg_staging_root_${NAME_SUFFIX}"

# Print variables for debugging
echo "--- Build Configuration (Universal PKG) ---"
echo "Version/Identifier: ${VERSION}"
echo "Build Type (Suffix): ${NAME_SUFFIX}"
echo "Installer Signer: ${INSTALLER_CERT_NAME}"
echo "Derived App Signer: ${APP_CERT_NAME}" # NEW
echo "Signed PKG Output: ${SIGNED_PKG_OUTPUT_PATH}"
echo "-------------------------------------------"

# --- 2. BUILD THE MAIN RUST TUI BINARIES (FOR BOTH ARCHS) ---
echo "Building main TUI binary for Apple Silicon (aarch64)..."
cargo build --target aarch64-apple-darwin --release "$@"

echo "Building main TUI binary for Intel (x86_64)..."
cargo build --target x86_64-apple-darwin --release "$@"

# --- 3. CREATE UNIVERSAL (FAT) BINARY ---
# --- Safety Check: Binaries ---
if [ ! -f "${TUI_BINARY_SOURCE_ARM64}" ] || [ ! -f "${TUI_BINARY_SOURCE_X86_64}" ]; then
    echo "::error:: One or more built binaries missing. Build failed."
    ls -l target/*/release || true
    exit 1
fi

echo "Creating universal (FAT) binary with lipo..."
rm -rf "${UNIVERSAL_STAGING_DIR}"
mkdir -p "${UNIVERSAL_STAGING_DIR}"
lipo -create \
  -output "${UNIVERSAL_BINARY_PATH}" \
  "${TUI_BINARY_SOURCE_ARM64}" \
  "${TUI_BINARY_SOURCE_X86_64}"

echo "Signing universal binary ${UNIVERSAL_BINARY_PATH} with Hardened Runtime..."
codesign -s "${APP_CERT_NAME}" \
  -v --force \
  --options runtime \
  --timestamp \
  "${UNIVERSAL_BINARY_PATH}"

# --- 4. CREATE THE MAGNET/TORRENT HANDLER APP ---
echo "Building ${HANDLER_APP_NAME}.app programmatically..."
rm -rf "${HANDLER_STAGING_DIR}"
mkdir -p "${HANDLER_APP_PATH}/Contents/MacOS"
mkdir -p "${HANDLER_APP_PATH}/Contents/Resources"

echo "Building universal native protocol handler..."
xcrun clang \
  -fobjc-arc \
  -fblocks \
  -Wall \
  -Wextra \
  -Werror \
  -Wno-deprecated-declarations \
  -mmacosx-version-min=10.13 \
  -arch arm64 \
  -arch x86_64 \
  -framework AppKit \
  -framework CoreServices \
  -weak_framework UniformTypeIdentifiers \
  -o "${HANDLER_EXECUTABLE}" \
  "${HANDLER_SOURCE}"

echo "Adding custom icon to ${HANDLER_APP_NAME}.app..."
RESOURCES_PATH="${HANDLER_APP_PATH}/Contents/Resources"
cp "${ICON_FILE_PATH}" "${RESOURCES_PATH}/${ICON_FILE_NAME}"
echo "Custom icon added."

PLIST_PATH="${HANDLER_APP_PATH}/Contents/Info.plist"
cp "${HANDLER_PLIST_SOURCE}" "${PLIST_PATH}"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${BUNDLE_VERSION}" "${PLIST_PATH}"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${BUNDLE_VERSION}" "${PLIST_PATH}"

plutil -lint "${PLIST_PATH}"
if /usr/libexec/PlistBuddy -c "Print :CFBundleIconName" "${PLIST_PATH}" &>/dev/null; then
  echo "::error:: Generated handler still contains the AppleScript droplet icon name."
  exit 1
fi
if [ -f "${RESOURCES_PATH}/Assets.car" ]; then
  echo "::error:: Generated handler still contains the AppleScript droplet asset catalog."
  exit 1
fi
if plutil -extract CFBundleDocumentTypes json -o - "${PLIST_PATH}" | grep -q '"\\*"'; then
  echo "::error:: Generated handler still claims the wildcard document type."
  exit 1
fi
if ! lipo -archs "${HANDLER_EXECUTABLE}" | grep -q 'x86_64 arm64\|arm64 x86_64'; then
  echo "::error:: Generated handler is not universal."
  exit 1
fi

echo "Signing ${HANDLER_APP_NAME}.app with Developer ID and Hardened Runtime..."
codesign -s "${APP_CERT_NAME}" \
  -v --force \
  --options runtime \
  --timestamp \
  "${HANDLER_EXECUTABLE}"
codesign -s "${APP_CERT_NAME}" \
  -v --force \
  --options runtime \
  --timestamp \
  "${HANDLER_APP_PATH}"
codesign --verify --deep --strict --verbose=2 "${HANDLER_APP_PATH}"

# --- 5. PREPARE STAGING ROOT FOR PKG ---
echo "Staging files for PKG installer..."
rm -rf "${PKG_STAGING_ROOT}"
mkdir -p "${PKG_STAGING_ROOT}/usr/local/bin"
mkdir -p "${PKG_STAGING_ROOT}/Applications"
cp "${UNIVERSAL_BINARY_PATH}" "${PKG_STAGING_ROOT}/usr/local/bin/"
cp -R "${HANDLER_APP_PATH}" "${PKG_STAGING_ROOT}/Applications/"

# --- 6. CREATE AND SIGN THE FINAL PKG ---
echo "Creating (unsigned) PKG at ${UNSIGNED_PKG_OUTPUT_PATH}..."
mkdir -p "${PKG_OUTPUT_DIR}"
pkgbuild \
  --root "${PKG_STAGING_ROOT}" \
  --scripts "${PKG_SCRIPTS_PATH}" \
  --install-location "/" \
  --identifier "${PKG_IDENTIFIER}" \
  --version "${VERSION}" \
  "${UNSIGNED_PKG_OUTPUT_PATH}"

echo "Signing PKG with '${INSTALLER_CERT_NAME}'..."
productsign --sign "${INSTALLER_CERT_NAME}" \
  "${UNSIGNED_PKG_OUTPUT_PATH}" \
  "${SIGNED_PKG_OUTPUT_PATH}"
  
# --- 7. CLEAN UP ---
rm -rf "${HANDLER_STAGING_DIR}"
rm -rf "${PKG_STAGING_ROOT}"
rm -rf "${UNIVERSAL_STAGING_DIR}"
rm -f "${UNSIGNED_PKG_OUTPUT_PATH}" # Remove the unsigned original

echo ""
echo "Signed PKG creation complete at: ${SIGNED_PKG_OUTPUT_PATH}"
echo "--------------------------------------------------------"
echo "PKG_PATH=${SIGNED_PKG_OUTPUT_PATH}" # Output for GitHub Actions
echo "PKG_NAME=${PKG_NAME}" # Output the filename
