#!/bin/sh
# Keep all work inside this function so incomplete piped downloads do not install.
install_main() {
    set -eu
    if [ "$(id -u)" -eq 0 ]; then
        printf '%s\n' 'Run this installer as your regular user, not root.' >&2
        exit 1
    fi
    [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || {
        printf '%s\n' 'Only x86_64 Linux is supported.' >&2; exit 1;
    }
    action=install
    archive=
    case "${1:-}" in
        --uninstall) action=uninstall; shift ;;
        --check-deps) action=check; shift ;;
        --archive) archive=${2:?Provide a local release tarball}; shift 2 ;;
        --help|-h)
            printf '%s\n' 'Usage: sh install.sh [--archive FILE | --check-deps | --uninstall]' \
                'Local archives require FILE.sha256 alongside them. No library or account data is removed.'
            exit 0 ;;
    esac
    [ "$#" -eq 0 ] || { printf '%s\n' 'Unknown option. Use --help.' >&2; exit 1; }
    : "${HOME:?HOME must be set}"
    data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
    private_dir=$HOME/.local/share/yt-music-gpui
    manager=unknown
    for candidate in pacman apt-get dnf zypper; do
        if command -v "$candidate" >/dev/null 2>&1; then manager=$candidate; break; fi
    done
    packages() {
        case "$manager" in
            pacman) set -- pacman -S --needed "$@" ;;
            apt-get) set -- apt-get install "$@" ;;
            dnf) set -- dnf install "$@" ;;
            zypper) set -- zypper install "$@" ;;
            *) printf 'Install these dependencies with your package manager: %s\n' "$*"; return 1 ;;
        esac
        printf '\nMissing dependencies. Exact command:\n  sudo %s\n' "$*"
        [ "$action" != check ] || return 1
        answer=n
        if [ -r /dev/tty ] && [ -w /dev/tty ]; then
            { printf 'Run it now? [y/N] ' > /dev/tty; read -r answer < /dev/tty; } 2>/dev/null || answer=n
        fi
        case "$answer" in y|Y|yes|YES) sudo "$@" ;; *) return 1 ;; esac
    }
    if ! python3 -I -c 'import sys; sys.exit(sys.version_info < (3, 10))' 2>/dev/null; then
        case "$manager" in pacman) python_package=python ;; *) python_package=python3 ;; esac
        packages "$python_package" || exit 1
        python3 -I -c 'import sys; sys.exit(sys.version_info < (3, 10))' || {
            printf '%s\n' 'Python 3.10 or newer is required.' >&2; exit 1;
        }
    fi
    umask 077
    work=$(mktemp -d)
    trap 'rm -rf -- "$work"' EXIT
    trap 'exit 1' HUP INT TERM
    cat > "$work/helper.py" <<'PY'
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import tarfile
import zipfile

home = Path(os.environ['HOME'])
data = Path(os.environ.get('XDG_DATA_HOME') or home / '.local/share')
private = home / '.local/share/yt-music-gpui'
licenses = ['LICENSE', 'LICENSE-APACHE', 'LICENSE-FEATHER', 'LICENSE-LUCIDE',
            'LICENSE-VIEWPORTER', 'NOTICE', 'OFL.txt']
targets = {
    'yt-music-gpui': home / '.local/bin/yt-music-gpui',
    'yt-music-gpui.desktop': data / 'applications/yt-music-gpui.desktop',
    'yt-music-gpui.svg': data / 'icons/hicolor/scalable/apps/yt-music-gpui.svg',
    **{name: data / 'licenses/yt-music-gpui' / name for name in licenses},
    'yt-dlp': private / 'yt-dlp', 'deno': private / 'deno',
}
manifest = private / 'install-manifest.json'

def digest(content):
    return hashlib.sha256(content).hexdigest()

def verify(path, checksum):
    if not re.fullmatch('[0-9a-f]{64}', checksum) or digest(Path(path).read_bytes()) != checksum:
        sys.exit(f'SHA-256 mismatch: {Path(path).name}')

# Anchor every write/unlink to a checked directory descriptor. O_NOFOLLOW rejects
# redirected parents, and the descriptor prevents later renames redirecting IO.
def directory(path, create=False):
    if not path.is_absolute() or '..' in path.parts:
        raise ValueError('Install directories must be absolute and contain no ..')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:]:
            if create:
                try:
                    os.mkdir(part, 0o755, dir_fd=fd)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            if info.st_uid not in (0, os.geteuid()) or (
                info.st_mode & 0o022 and not (info.st_uid == 0 and info.st_mode & stat.S_ISVTX)
            ):
                raise ValueError(f'Unsafe install directory: {path}')
        return fd
    except BaseException:
        os.close(fd)
        raise

def read_file(path):
    parent = directory(path.parent)
    try:
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
        with os.fdopen(fd, 'rb') as file:
            info = os.fstat(file.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
                raise ValueError(f'Unsafe installed file: {path}')
            return file.read()
    finally:
        os.close(parent)

def atomic_write(path, content, mode):
    fd = directory(path.parent, create=True)
    name = '.yt-music-install-' + secrets.token_hex(12)
    try:
        out = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode, dir_fd=fd)
        with os.fdopen(out, 'wb') as file:
            file.write(content)
            os.fchmod(file.fileno(), mode)
            file.flush()
            os.fsync(file.fileno())
        os.replace(name, path.name, src_dir_fd=fd, dst_dir_fd=fd)
    finally:
        try:
            os.unlink(name, dir_fd=fd)
        except FileNotFoundError:
            pass
        os.close(fd)

def load_manifest():
    try:
        saved = json.loads(read_file(manifest))
    except FileNotFoundError:
        return {}
    if not isinstance(saved, dict) or not saved.keys() <= targets.keys() or any(
        not isinstance(value, dict) or set(value) != {'path', 'sha256'}
        or value['path'] != str(targets[key])
        or not re.fullmatch('[0-9a-f]{64}', value['sha256']) for key, value in saved.items()
    ):
        raise ValueError('Invalid install manifest, or XDG_DATA_HOME changed since installation')
    return saved

try:
    action, *args = sys.argv[1:]
    if action == 'unpack':
        archive, sums, stage = map(Path, args)
        lines = [line.split() for line in sums.read_text().splitlines()]
        checksums = [line[0] for line in lines if len(line) == 2 and line[1] == archive.name]
        if len(checksums) != 1:
            raise ValueError('Expected exactly one checksum for the release tarball')
        verify(archive, checksums[0])
        expected = {'yt-music-gpui', 'yt-music-gpui.desktop', 'yt-music-gpui.svg',
                    'runtime-pins.json', *licenses}
        with tarfile.open(archive, 'r:gz') as tar:
            entries = tar.getmembers()
            if (len(entries) != len(expected) or {e.name for e in entries} != expected
                    or any(not e.isfile() or e.size > 200_000_000 for e in entries)
                    or sum(e.size for e in entries) > 250_000_000):
                raise ValueError('Unsafe release archive: expected regular files only, without paths or links')
            stage.mkdir()
            for entry in entries:
                (stage / entry.name).write_bytes(tar.extractfile(entry).read())
        pins = json.loads((stage / 'runtime-pins.json').read_text())
        if set(pins) != {'ytdlp_version', 'ytdlp_sha256', 'deno_version', 'deno_sha256', 'deno_size'}:
            raise ValueError('Invalid runtime pins')
        for key in ['ytdlp_version', 'deno_version']:
            if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', pins[key]):
                raise ValueError('Invalid runtime version')
        for key in ['ytdlp_sha256', 'deno_sha256']:
            if not re.fullmatch('[0-9a-f]{64}', pins[key]):
                raise ValueError('Invalid runtime checksum')
        if not isinstance(pins['deno_size'], int) or not 0 < pins['deno_size'] < 100_000_000:
            raise ValueError('Invalid Deno download size')
        print('\n'.join(str(pins[key]) for key in ['ytdlp_version', 'ytdlp_sha256', 'deno_version', 'deno_sha256', 'deno_size']))
    elif action == 'verify':
        path, checksum, sums, name = args
        verify(path, checksum)
        if sums != '-':
            matches = [line.split() for line in Path(sums).read_text().splitlines()
                       if len(line.split()) == 2 and line.split()[1] == name]
            if matches != [[checksum, name]]:
                raise ValueError('Upstream checksum differs from the reviewed pin')
    elif action == 'deno':
        archive, output = map(Path, args)
        with zipfile.ZipFile(archive) as z:
            entries = z.infolist()
            if (len(entries) != 1 or entries[0].filename != 'deno'
                    or stat.S_ISLNK(entries[0].external_attr >> 16)
                    or entries[0].file_size > 200_000_000):
                raise ValueError('Unsafe Deno archive')
            output.write_bytes(z.read(entries[0]))
    elif action == 'runtime':
        sys.path.insert(0, args[0])
        from yt_dlp.utils._jsruntime import DenoJsRuntime, NodeJsRuntime, BunJsRuntime, QuickJsRuntime
        runtimes = [DenoJsRuntime(), NodeJsRuntime(), BunJsRuntime(), QuickJsRuntime()]
        try:
            read_file(private / 'deno')
            runtimes.insert(0, DenoJsRuntime(str(private / 'deno')))
        except (OSError, ValueError):
            pass
        for runtime in runtimes:
            info = runtime.info
            if info and info.supported and not (info.name == 'bun' and info.version_tuple > (1, 3, 14)):
                print(f'Using {info.name} {info.version}')
                break
        else:
            sys.exit(1)
    elif action == 'libraries':
        for name in ['mpv.so.2', 'EGL.so.1', 'GLESv2.so.2', 'X11.so.6', 'Xext.so.6',
                     'wayland-client.so.0', 'wayland-egl.so.1', 'xcb.so.1',
                     'xkbcommon.so.0', 'xkbcommon-x11.so.0', 'fontconfig.so.1', 'freetype.so.6', 'vulkan.so.1']:
            try:
                ctypes.CDLL('lib' + name)
            except OSError:
                print(name.split('.')[0])
    elif action == 'install':
        stage, ytdlp, deno = map(Path, args)
        saved = load_manifest()
        sources = {name: stage / name for name in targets if name not in ('yt-dlp', 'deno')}
        sources['yt-dlp'] = ytdlp
        if deno.is_file():
            sources['deno'] = deno
        # Refuse to take ownership of existing files installed by another method.
        for name in sources:
            dest = targets[name]
            if os.path.lexists(dest) and name not in saved:
                raise ValueError(f'Existing file is not in the install manifest: {dest}')
            fd = directory(dest.parent, create=True)
            os.close(fd)
        for name, source in sources.items():
            content = source.read_bytes()
            mode = 0o755 if name in ('yt-music-gpui', 'yt-dlp', 'deno') else 0o644
            atomic_write(targets[name], content, mode)
            saved[name] = {'path': str(targets[name]), 'sha256': digest(content)}
            # Record each completed file so an interrupted install is recoverable.
            atomic_write(manifest, json.dumps(saved).encode(), 0o600)
    elif action == 'uninstall':
        saved = load_manifest()
        kept = {}
        for name, entry in saved.items():
            dest = targets[name]
            try:
                if digest(read_file(dest)) != entry['sha256']:
                    print(f'Keeping modified file: {dest}')
                    kept[name] = entry
                    continue
                fd = directory(dest.parent)
                try:
                    os.unlink(dest.name, dir_fd=fd)
                finally:
                    os.close(fd)
            except FileNotFoundError:
                pass
        if kept:
            atomic_write(manifest, json.dumps(kept).encode(), 0o600)
        elif saved:
            fd = directory(manifest.parent)
            try:
                os.unlink(manifest.name, dir_fd=fd)
            finally:
                os.close(fd)
    else:
        raise ValueError('Unknown installer operation')
except (OSError, ValueError, KeyError, tarfile.TarError, zipfile.BadZipFile) as error:
    sys.exit(str(error))
PY
    helper() { python3 -I -B "$work/helper.py" "$@"; }
    desktop_update() {
        if command -v update-desktop-database >/dev/null 2>&1; then
            update-desktop-database "$data_home/applications"
        fi
    }
    if [ "$action" = uninstall ]; then
        helper uninstall
        desktop_update
        printf '%s\n' 'Uninstalled. Library, settings and saved account were kept.'
        exit 0
    fi
    download() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
    if [ "$action" = install ]; then
        command -v curl >/dev/null 2>&1 || { packages curl || exit 1; }
        if [ -n "$archive" ]; then
            cp -- "$archive" "$work/$(basename -- "$archive")"
            cp -- "$archive.sha256" "$work/archive.sha256"
            archive=$work/$(basename -- "$archive")
        else
            # Resolve latest once, so a release published mid-install cannot mix assets.
            effective=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' \
                https://github.com/AksharP5/yt-music-gpui/releases/latest)
            tag=${effective##*/}
            version=${tag#v}
            case "$version" in ''|*[!0-9.]*) printf '%s\n' 'Invalid latest release version.' >&2; exit 1 ;; esac
            name=yt-music-gpui-$version-x86_64-linux.tar.gz
            url=https://github.com/AksharP5/yt-music-gpui/releases/download/$tag
            archive=$work/$name
            download "$url/$name" "$archive"
            download "$url/$name.sha256" "$work/archive.sha256"
        fi
        helper unpack "$archive" "$work/archive.sha256" "$work/stage" > "$work/pins"
        { read -r ytdlp_version; read -r ytdlp_hash; read -r deno_version; read -r deno_hash; read -r deno_size; } < "$work/pins"
        download "https://github.com/yt-dlp/yt-dlp/releases/download/$ytdlp_version/yt-dlp" "$work/yt-dlp"
        download "https://github.com/yt-dlp/yt-dlp/releases/download/$ytdlp_version/SHA2-256SUMS" "$work/ytdlp-sums"
        helper verify "$work/yt-dlp" "$ytdlp_hash" "$work/ytdlp-sums" yt-dlp
        runtime_zip=$work/yt-dlp
    else
        runtime_zip=$private_dir/yt-dlp
    fi
    helper libraries > "$work/missing"
    for tool in xdg-open secret-tool; do
        command -v "$tool" >/dev/null 2>&1 || printf '%s\n' "$tool" >> "$work/missing"
    done
    set --
    while IFS= read -r dep; do
        case "$manager:$dep" in
            pacman:mpv) pkg=mpv ;; apt-get:mpv|zypper:mpv) pkg=libmpv2 ;; dnf:mpv) pkg=mpv-libs ;;
            pacman:EGL|pacman:GLESv2|dnf:EGL|dnf:GLESv2) pkg=libglvnd ;; apt-get:EGL|zypper:EGL) pkg=libegl1 ;;
            apt-get:GLESv2|zypper:GLESv2) pkg=libgles2 ;;
            pacman:X11) pkg=libx11 ;; apt-get:X11) pkg=libx11-6 ;; dnf:X11|zypper:X11) pkg=libX11 ;;
            pacman:Xext) pkg=libxext ;; apt-get:Xext) pkg=libxext6 ;; dnf:Xext|zypper:Xext) pkg=libXext ;;
            pacman:wayland-*|dnf:wayland-*|zypper:wayland-*) pkg=wayland ;;
            apt-get:wayland-client) pkg=libwayland-client0 ;; apt-get:wayland-egl) pkg=libwayland-egl1 ;;
            pacman:xcb|dnf:xcb|zypper:xcb) pkg=libxcb ;; apt-get:xcb) pkg=libxcb1 ;;
            pacman:xkbcommon|dnf:xkbcommon|zypper:xkbcommon) pkg=libxkbcommon ;; apt-get:xkbcommon) pkg=libxkbcommon0 ;;
            pacman:xkbcommon-x11|dnf:xkbcommon-x11|zypper:xkbcommon-x11) pkg=libxkbcommon-x11 ;;
            apt-get:xkbcommon-x11) pkg=libxkbcommon-x11-0 ;;
            pacman:fontconfig|dnf:fontconfig|zypper:fontconfig) pkg=fontconfig ;; apt-get:fontconfig) pkg=libfontconfig1 ;;
            pacman:freetype) pkg=freetype2 ;; apt-get:freetype) pkg=libfreetype6 ;; dnf:freetype|zypper:freetype) pkg=freetype ;;
            pacman:vulkan) pkg=vulkan-icd-loader ;; apt-get:vulkan|zypper:vulkan) pkg=libvulkan1 ;; dnf:vulkan) pkg=vulkan-loader ;;
            *:xdg-open) pkg=xdg-utils ;;
            pacman:secret-tool) pkg=libsecret ;; apt-get:secret-tool) pkg=libsecret-tools ;; dnf:secret-tool|zypper:secret-tool) pkg=libsecret ;;
            *) printf 'Missing runtime library: %s\n' "$dep" >&2; exit 1 ;;
        esac
        case " $* " in *" $pkg "*) ;; *) set -- "$@" "$pkg" ;; esac
    done < "$work/missing"
    if [ "$#" -gt 0 ]; then
        packages "$@" || exit 1
        helper libraries > "$work/missing"
        [ ! -s "$work/missing" ] && command -v xdg-open >/dev/null && command -v secret-tool >/dev/null || exit 1
    fi
    if ! helper runtime "$runtime_zip"; then
        if [ "$action" = check ]; then
            printf '%s\n' 'No supported JavaScript runtime. Re-run the installer to add private Deno.' >&2
            exit 1
        fi
        printf 'No supported JavaScript runtime found. Installing private Deno %s (%s bytes download).\n' "$deno_version" "$deno_size"
        download "https://github.com/denoland/deno/releases/download/v$deno_version/deno-x86_64-unknown-linux-gnu.zip" "$work/deno.zip"
        helper verify "$work/deno.zip" "$deno_hash" - deno
        helper deno "$work/deno.zip" "$work/deno"
    fi
    if [ "$action" = check ]; then printf '%s\n' 'Runtime dependencies found.'; exit 0; fi
    helper install "$work/stage" "$work/yt-dlp" "$work/deno"
    desktop_update
    printf '%s\n' 'YouTube Music is installed. Launch it from your app launcher.'
    case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) printf 'For terminal use, add %s/.local/bin to PATH.\n' "$HOME" ;; esac
}
install_main "$@"
