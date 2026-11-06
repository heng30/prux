#!/bin/bash
# 把 target/release/prux-gui 打成 deb，产物 target/prux-gui-<version>-x86_64-linux.deb。
# 先把骨架拷到 target/deb-prux-gui 再往里加二进制与图标，仓库里的 package/deb 保持干净。
set -euo pipefail

DIR=$(dirname "$(readlink -f "$0")")
ROOT_DIR=$(cd "$DIR/../.." && pwd)
APP_NAME="prux-gui"
ICON_NAME="prux"
BIN_PATH="$ROOT_DIR/target/release/$APP_NAME"
ICON_SRC="$ROOT_DIR/icon.png"
STAGE="$ROOT_DIR/target/deb-$APP_NAME"
SIZES=(16x16 22x22 24x24 32x32 36x36 48x48 64x64 72x72 96x96 128x128 192x192 256x256 512x512)

if [ ! -f "$BIN_PATH" ]; then
    echo "Error: $BIN_PATH not found. Run 'make release-gui' first." >&2
    exit 1
fi

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n1)
if [ -z "$version" ]; then
    echo "Error: cannot read version from Cargo.toml." >&2
    exit 1
fi

# 运行时依赖按二进制自己的 NEEDED 生成：库名 → Debian 包名，未列出的库只告警不写进 Depends
pkg_deps=""
while IFS= read -r lib; do
    case "$lib" in
        libxkbcommon.so.0)      pkg_deps="$pkg_deps, libxkbcommon0" ;;
        libxkbcommon-x11.so.0)  pkg_deps="$pkg_deps, libxkbcommon-x11-0" ;;
        libxcb.so.1)            pkg_deps="$pkg_deps, libxcb1" ;;
        libxcb-randr.so.0)      pkg_deps="$pkg_deps, libxcb-randr0" ;;
        libwayland-client.so.0) pkg_deps="$pkg_deps, libwayland-client0" ;;
        libwayland-cursor.so.0) pkg_deps="$pkg_deps, libwayland-cursor0" ;;
        libEGL.so.1)            pkg_deps="$pkg_deps, libegl1" ;;
        libGL.so.1)             pkg_deps="$pkg_deps, libgl1" ;;
        libgbm.so.1)            pkg_deps="$pkg_deps, libgbm1" ;;
        libfontconfig.so.1)     pkg_deps="$pkg_deps, libfontconfig1" ;;
        libfreetype.so.6)       pkg_deps="$pkg_deps, libfreetype6" ;;
        libpipewire-0.3.so.0)   pkg_deps="$pkg_deps, libpipewire-0.3-0" ;;
        libasound.so.2)         pkg_deps="$pkg_deps, libasound2" ;;
        libdbus-1.so.3)         pkg_deps="$pkg_deps, libdbus-1-3" ;;
        libz.so.1)              pkg_deps="$pkg_deps, zlib1g" ;;
        libgcc_s.so.1)          pkg_deps="$pkg_deps, libgcc-s1" ;;
        libstdc++.so.6)         pkg_deps="$pkg_deps, libstdc++6" ;;
        libc.so.6|libm.so.6|libdl.so.2|libpthread.so.0|librt.so.1|ld-linux-*.so.*) ;;
        *) echo "warning: no Debian package known for $lib, skipped" >&2 ;;
    esac
done < <(objdump -p "$BIN_PATH" | grep NEEDED | awk '{print $2}' | sort -u)
pkg_deps=${pkg_deps#, }

rm -rf "$STAGE"
cp -r "$DIR/package" "$STAGE"

mkdir -p "$STAGE/usr/bin"
install -m 0755 "$BIN_PATH" "$STAGE/usr/bin/$APP_NAME"

sed -i "s/^Version:.*/Version: $version/; s/^Depends:.*/Depends: $pkg_deps/" "$STAGE/DEBIAN/control"

icon_tool=""
if command -v magick >/dev/null; then
    icon_tool="magick"
elif command -v convert >/dev/null; then
    icon_tool="convert"
fi
if [ -n "$icon_tool" ]; then
    for size in "${SIZES[@]}"; do
        dest="$STAGE/usr/share/icons/hicolor/$size/apps"
        mkdir -p "$dest"
        $icon_tool "$ICON_SRC" -resize "$size" -background none -gravity center -extent "$size" "$dest/$ICON_NAME.png"
    done
else
    # 没装 ImageMagick 就只放一张 256x256，图标尺寸表不全但仍能显示
    mkdir -p "$STAGE/usr/share/icons/hicolor/256x256/apps"
    cp "$ICON_SRC" "$STAGE/usr/share/icons/hicolor/256x256/apps/$ICON_NAME.png"
fi

chmod 0755 "$STAGE/DEBIAN"
chmod 0644 "$STAGE/DEBIAN/control" "$STAGE/DEBIAN/copyright"

output="$ROOT_DIR/target/$APP_NAME-$version-x86_64-linux.deb"
rm -f "$output"
dpkg-deb --root-owner-group --build "$STAGE" "$output"
echo "built: $output"
