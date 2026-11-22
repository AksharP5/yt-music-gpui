#!/usr/bin/env bash
# Builds the LGPL libmpv the macOS app ships: mpv without its GPL parts, and
# FFmpeg without GPL or nonfree code, linked with libplacebo, libass, dav1d and
# libass's font libraries into one libmpv.2.dylib that loads only macOS
# frameworks. Sources are pinned by SHA-256. Fails unless FFmpeg and mpv are
# both configured as LGPL.
#
#   packaging/macos-libmpv.sh PREFIX
#
# Needs Xcode's command line tools, pkg-config, and meson, ninja and Python with
# Jinja2 on PATH, as pinned in packaging/macos-build-requirements.txt. Writes the library and
# headers to PREFIX, and each project's license files and source to
# PREFIX/share/licenses.
set -euo pipefail

prefix=$(mkdir -p "$1" && cd "$1" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
downloads="$root/target/macos-downloads"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Deno, also in the app, already needs macOS 12.
export MACOSX_DEPLOYMENT_TARGET=12.0
# Only the libraries built here, never Homebrew's or the system's.
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
jobs=${CARGO_BUILD_JOBS:-4}

listing="$prefix/share/licenses/LIBRARIES.txt"
mkdir -p "$downloads" "$prefix/share/licenses"
cat > "$listing" <<'EOF'
libmpv.2.dylib in Contents/Frameworks is built from these sources, linked
statically, by packaging/macos-libmpv.sh. It loads only macOS frameworks.
Each project's license files are in licenses/<project>.

EOF

# fetch NAME VERSION LICENSE URL SHA256 LICENSE_FILE...: unpacks into $work/NAME.
fetch() {
    local name=$1 version=$2 license=$3 url=$4 sha256=$5
    shift 5
    local archive="$downloads/$sha256"
    if [[ ! -f "$archive" ]] || ! echo "$sha256  $archive" | shasum -a 256 -c - >/dev/null; then
        curl -q --proto '=https' --tlsv1.2 -fsSL "$url" -o "$archive.part"
        echo "$sha256  $archive.part" | shasum -a 256 -c -
        mv "$archive.part" "$archive"
    fi
    mkdir "$work/$name"
    tar -xf "$archive" -C "$work/$name" --strip-components 1
    mkdir -p "$prefix/share/licenses/$name"
    for file in "$@"; do
        cp "$work/$name/$file" "$prefix/share/licenses/$name/"
    done
    printf '%s %s: %s\n  Source: %s\n  SHA-256: %s\n' \
        "$name" "$version" "$license" "$url" "$sha256" >> "$listing"
}

printf '[binaries]\npkg-config = [%s, %s]\n' "'pkg-config'" "'--static'" > "$work/native.ini"
# meson_install NAME static|shared OPTION...
meson_install() {
    local name=$1 library=$2
    shift 2
    meson setup "$work/$name/build" "$work/$name" --prefix="$prefix" --libdir=lib \
        --buildtype=release --default-library="$library" --wrap-mode=nodownload \
        --native-file="$work/native.ini" "$@"
    ninja -C "$work/$name/build" -j "$jobs" install
}

fetch dav1d 1.5.5 BSD-2-Clause \
    https://code.videolan.org/videolan/dav1d/-/archive/1.5.5/dav1d-1.5.5.tar.bz2 \
    afc5378daba30eafd11d7b4f0ec863270d7b175f38393a2e1e0ff76f3a45c8b9 COPYING
meson_install dav1d static -Denable_tools=false -Denable_examples=false -Denable_tests=false

fetch freetype 2.15.0 FTL \
    https://downloads.sourceforge.net/project/freetype/freetype2/2.15.0/freetype-2.15.0.tar.xz \
    93cd47eb719c325107f2d3cf087d06299e944c3b48a40b55537302304dea1dd5 LICENSE.TXT docs/FTL.TXT
meson_install freetype static -Dbrotli=disabled -Dbzip2=disabled -Dharfbuzz=disabled \
    -Dpng=disabled -Dzlib=disabled -Dtests=disabled

fetch fribidi 1.0.18 LGPL-2.1-or-later \
    https://github.com/fribidi/fribidi/releases/download/v1.0.18/fribidi-1.0.18.tar.xz \
    1de5e8073d035388b71d0b2d452b1da1f8caeea8a7702997b401fe1b6250727f COPYING
meson_install fribidi static -Ddocs=false -Dbin=false -Dtests=false

fetch harfbuzz 15.2.0 MIT \
    https://github.com/harfbuzz/harfbuzz/releases/download/15.2.0/harfbuzz-15.2.0.tar.xz \
    e2ec49090744383cdbe7df808c1cedaafba06a97be8dc4211fe546ba585f12cb COPYING
meson_install harfbuzz static -Dglib=disabled -Dgobject=disabled -Dcairo=disabled -Dchafa=disabled \
    -Dpng=disabled -Dzlib=disabled -Dicu=disabled -Dgraphite2=disabled -Dfreetype=disabled \
    -Dcoretext=disabled -Dsubset=disabled -Dtests=disabled -Dintrospection=disabled \
    -Ddocs=disabled -Dutilities=disabled -Dbenchmark=disabled

fetch libass 0.17.5 ISC \
    https://github.com/libass/libass/releases/download/0.17.5/libass-0.17.5.tar.xz \
    2dca25c0e0c837ddf00b52011b3f82cac1e4ddd3ad018227806b0c2288864acc COPYING
meson_install libass static -Dfontconfig=disabled -Dcoretext=enabled -Dlibunibreak=disabled

fetch ffmpeg 9.0.2 LGPL-2.1-or-later \
    https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz \
    8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e COPYING.LGPLv2.1 LICENSE.md
# Decoding, demuxing and https only. Nothing is detected implicitly; without
# --enable-gpl, --enable-version3 or --enable-nonfree FFmpeg stays LGPL 2.1.
(cd "$work/ffmpeg" && ./configure --prefix="$prefix" --pkg-config-flags=--static \
    --enable-static --disable-shared --enable-pic --disable-programs --disable-doc \
    --disable-debug --disable-avdevice --disable-encoders --disable-muxers \
    --disable-autodetect --enable-pthreads --enable-zlib --enable-securetransport \
    --enable-videotoolbox --enable-audiotoolbox --enable-libdav1d \
    --extra-cflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET")
grep -qx '#define FFMPEG_LICENSE "LGPL version 2.1 or later"' "$work/ffmpeg/config.h"
for gpl in CONFIG_GPL CONFIG_VERSION3 CONFIG_NONFREE; do
    grep -qx "#define $gpl 0" "$work/ffmpeg/config.h"
done
make -C "$work/ffmpeg" -j "$jobs" install

fetch libplacebo 7.387.0 LGPL-2.1-or-later \
    https://code.videolan.org/videolan/libplacebo/-/archive/v7.387.0/libplacebo-v7.387.0.tar.bz2 \
    14ceeeb14d1c08fcdb1a782a06129da11f842c6fddbf3a43c1c4061406ad03d2 LICENSE
# Header-only, and not in libplacebo's release archive. Used under its MIT option.
fetch fast_float 8.3.3 MIT \
    https://github.com/fastfloat/fast_float/archive/refs/tags/v8.3.3.tar.gz \
    4dd695f051893c8218faf2ed9871fa4a4990aa6aee2d3190ed7bfa2e83092245 LICENSE-MIT
cp -R "$work/fast_float/include" "$work/libplacebo/3rdparty/fast_float/"
# Declarations for libplacebo's Vulkan stubs, used under their MIT option.
fetch vulkan-headers 1.4.363.0 MIT \
    https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/vulkan-sdk-1.4.363.0.tar.gz \
    4a078be12bef21cfebc09d878b77a63cff9d68f899254a0b00d0e37ef73e7f7e LICENSES/MIT.txt
cp -R "$work/vulkan-headers/include" "$work/libplacebo/3rdparty/Vulkan-Headers/"
meson_install libplacebo static -Dvulkan=disabled -Dopengl=disabled -Dd3d11=disabled \
    -Dglslang=disabled -Dshaderc=disabled -Dlcms=disabled -Dlibdovi=disabled \
    -Ddemos=false -Dtests=false -Dunwind=disabled -Dxxhash=disabled

fetch mpv 0.42.0 LGPL-2.1-or-later \
    https://github.com/mpv-player/mpv/archive/refs/tags/v0.42.0.tar.gz \
    bcfebbfa2b65a6d88dc97ea18117831072373e38b5785a2afb874ed4df5070dd LICENSE.LGPL Copyright
# mpv 0.42's Swift view casts to a class it compiles only with Vulkan; its
# base class has the same isOpaque.
sed -i '' 's/layer as? MetalLayer {/layer as? CAMetalLayer {/' "$work/mpv/video/out/mac/view.swift"
echo '  Changed: video/out/mac/view.swift casts the layer to CAMetalLayer, not MetalLayer.' >> "$listing"
# The app renders through libmpv's OpenGL API, with VideoToolbox frames shared
# through IOSurface (videotoolbox-gl, which needs cocoa and its Swift layer).
meson_install mpv shared -Dgpl=false -Dlibmpv=true -Dcplayer=false -Dauto_features=disabled \
    -Dcocoa=enabled -Dswift-build=enabled -Dgl=enabled -Dgl-cocoa=enabled -Dplain-gl=enabled \
    -Dvideotoolbox-gl=enabled -Dcoreaudio=enabled -Dzlib=enabled -Diconv=enabled \
    -Dmacos-10-15-4-features=enabled -Dmacos-11-features=enabled \
    -Dmacos-11-3-features=enabled -Dmacos-12-features=enabled
grep -qx '#define HAVE_GPL 0' "$work/mpv/build/config.h"

# libmpv must load nothing but system libraries and frameworks. Swift's link
# step leaves an unused rpath into Xcode.
library="$prefix/lib/libmpv.2.dylib"
otool -l "$library" | awk '/cmd LC_RPATH/ { getline; getline; print $2 }' | grep -v '^/usr/lib/' |
    while read -r rpath; do install_name_tool -delete_rpath "$rpath" "$library"; done
if otool -L "$library" | tail -n +3 | grep -v -e '^[[:space:]]*/usr/lib/' -e '^[[:space:]]*/System/Library/'; then
    echo "libmpv loads a library from outside macOS" >&2
    exit 1
fi
echo "Built $library: FFmpeg $(grep -o 'LGPL version 2.1 or later' "$work/ffmpeg/config.h"), mpv -Dgpl=false"
