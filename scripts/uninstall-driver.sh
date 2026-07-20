#!/bin/sh
set -eu

source_bundle=/Library/Audio/Plug-Ins/HAL/VCableDriver.driver
removed_root=/Library/Application\ Support/VCable/Removed

if [ "$(id -u)" -ne 0 ]; then
    echo "uninstall-driver.sh must be run as root" >&2
    exit 1
fi

if [ "$#" -ne 0 ]; then
    echo "usage: uninstall-driver.sh" >&2
    exit 2
fi

if [ ! -d "$source_bundle" ]; then
    echo "driver is not installed: $source_bundle" >&2
    exit 1
fi

timestamp=$(/bin/date -u +%Y%m%dT%H%M%SZ)
destination="$removed_root/VCableDriver-$timestamp.driver"
/bin/mkdir -p "$removed_root"

if [ -e "$destination" ]; then
    echo "refusing to overwrite recovery bundle: $destination" >&2
    exit 1
fi

/bin/mv "$source_bundle" "$destination"
echo "moved driver to recoverable location: $destination"
echo "restart macOS to complete the uninstall"

