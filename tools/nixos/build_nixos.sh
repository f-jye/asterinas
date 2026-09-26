#!/bin/bash

# SPDX-License-Identifier: MPL-2.0

set -e

TARGET_ARCH=${TARGET_ARCH:-x86_64}
# Accept config file name as parameter, default to "configuration.nix"
CONFIG_FILE_NAME=${1:-"configuration.nix"}

# Defaults matching the Makefile, so the script also works standalone.
NIXOS_DISK_SIZE_IN_MB=${NIXOS_DISK_SIZE_IN_MB:-16384}
NIXOS_DISABLE_SYSTEMD=${NIXOS_DISABLE_SYSTEMD:-false}
NIXOS_STAGE_2_INIT=${NIXOS_STAGE_2_INIT:-"/bin/sh -l"}
RELEASE_SUBSTITUTER=${RELEASE_SUBSTITUTER:-https://aster-nixos-release.cachix.org}
DEV_SUBSTITUTER=${DEV_SUBSTITUTER:-https://aster-nixos-dev.cachix.org}
RELEASE_TRUSTED_PUBLIC_KEY=${RELEASE_TRUSTED_PUBLIC_KEY:-aster-nixos-release.cachix.org-1:xB6U/f5ck5vGDJZ04kPp3zGpZ4Nro9X4+TSSMAETVFE=}
DEV_TRUSTED_PUBLIC_KEY=${DEV_TRUSTED_PUBLIC_KEY:-aster-nixos-dev.cachix.org-1:xrCbE2flfliFTQCY/2HeJoT2tCO+5kMTZeLIUH9lnIA=}
LOG_LEVEL=${LOG_LEVEL:-error}
CONSOLE=${CONSOLE:-hvc0}

SCRIPT_DIR=$(cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd)
ASTERINAS_DIR=$(realpath ${SCRIPT_DIR}/../..)
ASTER_IMAGE_PATH=${ASTERINAS_DIR}/target/nixos/asterinas.img
DISTRO_DIR=$(realpath ${ASTERINAS_DIR}/distro)
CONFIG_PATH=${DISTRO_DIR}/etc_nixos/${CONFIG_FILE_NAME}

NIX_SYSTEM=$("${SCRIPT_DIR}/print_target_nix_system.sh" "${TARGET_ARCH}") || exit 1

pushd $DISTRO_DIR
nix-build aster_nixos_installer/default.nix \
    --argstr target_platform "${NIX_SYSTEM}" \
    --argstr disable-systemd "${NIXOS_DISABLE_SYSTEMD}" \
    --argstr stage-2-hook "${NIXOS_STAGE_2_INIT}" \
    --argstr log-level "${LOG_LEVEL}" \
    --argstr console "${CONSOLE}" \
    --argstr extra-substituters "${RELEASE_SUBSTITUTER} ${DEV_SUBSTITUTER}" \
    --argstr extra-trusted-public-keys "${RELEASE_TRUSTED_PUBLIC_KEY} ${DEV_TRUSTED_PUBLIC_KEY}"
popd

mkdir -p ${ASTERINAS_DIR}/target/nixos
if [ ! -e ${ASTER_IMAGE_PATH} ]; then
    echo "Creating image at ${ASTER_IMAGE_PATH} of size ${NIXOS_DISK_SIZE_IN_MB}MB......"
    fallocate -l ${NIXOS_DISK_SIZE_IN_MB}M ${ASTER_IMAGE_PATH}
    echo "Image created successfully!"
fi

DISK=$(losetup -fP --show ${ASTER_IMAGE_PATH})
cleanup() {
    losetup -d ${DISK} 2>/dev/null || true
}
trap cleanup EXIT INT TERM ERR

${DISTRO_DIR}/result/bin/aster-nixos-install --config ${CONFIG_PATH} --disk ${DISK}
