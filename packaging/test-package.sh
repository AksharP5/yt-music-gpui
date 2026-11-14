#!/bin/sh
# Installs the release .deb or .rpm from /dist in a clean distribution
# container, checks it, then removes it. Run as root inside the container:
#   docker run --rm -e YTDLP_VERSION=X -v "$PWD/dist:/dist:ro" \
#     -v "$PWD/packaging/test-package.sh:/test-package.sh:ro" ubuntu:24.04 sh /test-package.sh
set -eu
: "${YTDLP_VERSION:?Set YTDLP_VERSION to the reviewed yt-dlp version}"
if command -v apt-get >/dev/null; then
    apt-get update -qq
    apt-get install -y /dist/yt-music-gpui_*_amd64.deb
    files=$(dpkg -L yt-music-gpui | grep yt-music-gpui)
    remove='apt-get purge -y yt-music-gpui'
elif command -v dnf >/dev/null; then
    dnf install -y /dist/yt-music-gpui-*.x86_64.rpm
    files=$(rpm -ql yt-music-gpui | grep yt-music-gpui)
    remove='dnf remove -y yt-music-gpui'
else
    zypper --non-interactive install --allow-unsigned-rpm /dist/yt-music-gpui-*.x86_64.rpm
    files=$(rpm -ql yt-music-gpui | grep yt-music-gpui)
    remove='zypper --non-interactive remove yt-music-gpui'
fi
printf 'Installed:\n%s\n' "$files"

if ldd /usr/bin/yt-music-gpui | grep 'not found'; then exit 1; fi
# Check as an ordinary user, the way the app runs.
version() {
    setpriv --reuid=65534 --regid=65534 --clear-groups env HOME=/tmp yt-music-gpui --version | tee /tmp/version
}
version
grep -qx 'yt-dlp: /usr/lib/yt-music-gpui/yt-dlp' /tmp/version
grep -qx 'deno: /usr/lib/yt-music-gpui/deno' /tmp/version
chmod o+w /usr/lib/yt-music-gpui
version
grep -qx 'yt-dlp: not found' /tmp/version
grep -qx 'deno: from PATH' /tmp/version
chmod o-w /usr/lib/yt-music-gpui

[ "$(/usr/lib/yt-music-gpui/yt-dlp --version)" = "$YTDLP_VERSION" ]
cd /tmp
python3 -I -c '
import sys
sys.path.insert(0, "/usr/lib/yt-music-gpui/yt-dlp")
from yt_dlp.utils._jsruntime import DenoJsRuntime
info = DenoJsRuntime("/usr/lib/yt-music-gpui/deno").info
print("yt-dlp uses", info.name, info.version)
sys.exit(not info.supported)
'

$remove
for file in $files; do
    if [ -e "$file" ]; then echo "Left behind: $file"; exit 1; fi
done
echo 'Removed cleanly.'
