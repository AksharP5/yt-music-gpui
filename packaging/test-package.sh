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
    dpkg -s mesa-vulkan-drivers >/dev/null
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

# Run as an ordinary user, the way the app runs. Command substitution makes a
# crash or non-zero exit fail the script.
version() {
    out=$(python3 -I -c '
import os, sys
os.setgroups([]); os.setgid(65534); os.setuid(65534)
os.environ["HOME"] = sys.argv[2]
os.execv(sys.argv[1], [sys.argv[1], "--version"])
' "$1" "$2")
    printf '%s\n' "$out"
    printf '%s\n' "$out" | grep -q '^yt-music-gpui [0-9]'
}
expect() {
    printf '%s\n' "$out" | grep -qx "$1" || { echo "Expected: $1"; exit 1; }
}

# A trusted installer copy in HOME, to check which copy each install prefers.
home=/tmp/home
mkdir -p "$home/.local/share/yt-music-gpui"
printf '#!/bin/sh\n' > "$home/.local/share/yt-music-gpui/yt-dlp"
chmod 755 "$home" "$home/.local" "$home/.local/share" "$home/.local/share/yt-music-gpui" \
    "$home/.local/share/yt-music-gpui/yt-dlp"
chown -R 65534:65534 "$home"
version /usr/bin/yt-music-gpui "$home"
expect 'yt-dlp: /usr/lib/yt-music-gpui/yt-dlp'
expect 'deno: /usr/lib/yt-music-gpui/deno'
mkdir -p /usr/local/bin
cp /usr/bin/yt-music-gpui /usr/local/bin/yt-music-gpui
version /usr/local/bin/yt-music-gpui "$home"
expect "yt-dlp: $home/.local/share/yt-music-gpui/yt-dlp"
rm /usr/local/bin/yt-music-gpui

# Once others can write the directory, the app falls back to what is on PATH.
# Distribution packages may have pulled in their own yt-dlp.
chmod o+w /usr/lib/yt-music-gpui
version /usr/bin/yt-music-gpui /nonexistent
expect "yt-dlp: $(command -v yt-dlp || echo 'not found')"
expect "deno: $(command -v deno || echo 'not found')"
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
