#!/bin/sh
set -eu

target=/Library/Audio/Plug-Ins/HAL/VCableDriver.driver

if [ "$(id -u)" -ne 0 ]; then
    echo "install-driver.sh must be run as root" >&2
    exit 1
fi

if [ "$#" -ne 1 ]; then
    echo "usage: install-driver.sh PATH_TO_VCableDriver.driver" >&2
    exit 2
fi

source_bundle=$1
if [ ! -d "$source_bundle" ] || [ ! -f "$source_bundle/Contents/MacOS/VCableDriver" ]; then
    echo "source is not a VCableDriver.driver bundle: $source_bundle" >&2
    exit 1
fi

/usr/bin/codesign --verify --deep --strict --verbose=2 "$source_bundle"

if [ -e "$target" ]; then
    echo "refusing to overwrite existing driver: $target" >&2
    exit 1
fi

/usr/bin/ditto "$source_bundle" "$target"
/usr/sbin/chown -R root:wheel "$target"
/bin/chmod -R a+rX,go-w "$target"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$target"

echo "installed $target"
echo "restart macOS before using VCable"

