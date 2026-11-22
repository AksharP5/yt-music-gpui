#!/usr/bin/env python3
"""Builds `YouTube Music.app` and a .dmg with its .sha256 from a release build.

    packaging/macos-libmpv.sh target/libmpv
    LIBMPV_PREFIX=$PWD/target/libmpv cargo build --release --locked
    python3 packaging/macos-dmg.py [dist]

Build libmpv first with packaging/macos-libmpv.sh and link the app against it
(LIBMPV_PREFIX): the app runs without Homebrew, with that LGPL libmpv in
Contents/Frameworks. The build fails if libmpv isn't LGPL, or if anything in
the app loads a library from outside macOS. Contents/Resources holds the reviewed
yt-dlp zipapp, Deno, and a Python to run yt-dlp with, each checked against a
pinned SHA-256. The bundle is signed ad hoc: Apple Silicon runs no unsigned
code. Downloads are kept in target/macos-downloads.
"""

import ast
import hashlib
import plistlib
import re
import shutil
import subprocess
import sys
import tomllib
import urllib.request
import zipfile
from pathlib import Path

# python-build-standalone's stripped install-only CPython for Apple Silicon.
PYTHON_URL = (
    "https://github.com/astral-sh/python-build-standalone/releases/download/20261110/"
    "cpython-3.13.18%2B20261110-aarch64-apple-darwin-install_only_stripped.tar.gz"
)
PYTHON_SHA256 = "7a2384641584ab9984221fcb0fc9265df465922af71be71410529e2a9c3f0540"
# Never loaded by yt-dlp: pip, the IDE and Tk.
PYTHON_UNUSED = ["ensurepip", "idlelib", "site-packages", "tkinter", "_tkinter*", "turtledemo"]

APP_NAME = "YouTube Music"
BUNDLE_ID = "io.github.aksharp5.yt-music-gpui"
SYSTEM = ("/usr/lib/", "/System/Library/")

root = Path(__file__).resolve().parent.parent
dist = Path(sys.argv[1] if len(sys.argv) > 1 else "dist").resolve()
downloads = root / "target/macos-downloads"
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
source = ast.parse((root / "src/ytdlp_account.py").read_text())
pins = {
    node.targets[0].id: ast.literal_eval(node.value)
    for node in source.body
    if isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant)
}


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout


def download(url, sha256):
    path = downloads / sha256
    if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest() != sha256:
        downloads.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(url) as response:
            data = response.read()
        actual = hashlib.sha256(data).hexdigest()
        if actual != sha256:
            sys.exit(f"{url} has SHA-256 {actual}, expected {sha256}")
        path.write_bytes(data)
    return path


def is_macho(path):
    with open(path, "rb") as file:
        return file.read(4) in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe")


def linked(path):
    """The libraries a Mach-O file loads, without its own install name."""
    own = run("otool", "-D", str(path)).splitlines()[1:]
    lines = run("otool", "-L", str(path)).splitlines()[1:]
    return [line.split(" (")[0].strip() for line in lines if line.split(" (")[0].strip() not in own]


def load_commands(path):
    """Yields each load command of a Mach-O file as a dict of its fields."""
    command = {}
    for line in run("otool", "-l", str(path)).splitlines():
        if line.startswith("Load command"):
            if command:
                yield command
            command = {}
        elif line.strip():
            key, _, value = line.strip().partition(" ")
            command[key] = value.strip()
    if command:
        yield command


work = dist / "macos-build"
volume = work / "volume"
app = volume / f"{APP_NAME}.app"
contents = app / "Contents"
frameworks = contents / "Frameworks"
resources = contents / "Resources"
shutil.rmtree(work, ignore_errors=True)
for directory in [contents / "MacOS", frameworks, resources / "licenses"]:
    directory.mkdir(parents=True)

executable = contents / "MacOS/yt-music-gpui"
shutil.copy(root / "target/release/yt-music-gpui", executable)

# The app's only library outside macOS is the LGPL libmpv that
# packaging/macos-libmpv.sh builds, which loads nothing but macOS itself.
outside = [path for path in linked(executable) if not path.startswith(SYSTEM)]
if [Path(path).name for path in outside] != ["libmpv.2.dylib"]:
    sys.exit(f"The app must load only macOS and libmpv, not {outside}")
libmpv = Path(outside[0]).resolve()
prefix = libmpv.parent.parent
if not (prefix / "share/licenses/LIBRARIES.txt").is_file():
    sys.exit(f"{libmpv} is not from packaging/macos-libmpv.sh")
shutil.copy(libmpv, frameworks / "libmpv.2.dylib")
(frameworks / "libmpv.2.dylib").chmod(0o644)
run("install_name_tool", "-id", "@rpath/libmpv.2.dylib", str(frameworks / "libmpv.2.dylib"))
run("install_name_tool", "-change", outside[0], "@rpath/libmpv.2.dylib", str(executable))
run("install_name_tool", "-add_rpath", "@executable_path/../Frameworks", str(executable))
shutil.copytree(prefix / "share/licenses", resources / "licenses", dirs_exist_ok=True)

# FFmpeg embeds each library's license, and mpv its build options. Anything
# but LGPL 2.1 FFmpeg and mpv built with -Dgpl=false fails the build.
binary = (frameworks / "libmpv.2.dylib").read_bytes()
licenses = dict(re.findall(rb"(lib[a-z]+) license: ([^\0]+)\0", binary))
configuration = re.search(rb"Configuration: ([^\0]+)\0", binary)
for library, license in sorted(licenses.items()):
    print(f"{library.decode()}: {license.decode()}")
print(f"mpv: {configuration[1].decode() if configuration else 'no configuration'}")
if (
    not {b"libavcodec", b"libavformat", b"libavutil"} <= licenses.keys()
    or set(licenses.values()) != {b"LGPL version 2.1 or later"}
    or not configuration
    or b"-Dgpl=false" not in configuration[1].split()
):
    sys.exit("libmpv must be LGPL: FFmpeg without GPL parts and mpv with -Dgpl=false")

# The reviewed yt-dlp zipapp, Deno and Python.
ytdlp = download(
    f"https://github.com/yt-dlp/yt-dlp/releases/download/{pins['REVIEWED_VERSION']}/yt-dlp",
    pins["REVIEWED_SHA256"],
)
shutil.copy(ytdlp, resources / "yt-dlp")
deno = download(
    f"https://github.com/denoland/deno/releases/download/v{pins['DENO_VERSION']}/deno-aarch64-apple-darwin.zip",
    pins["DENO_MACOS_SHA256"],
)
with zipfile.ZipFile(deno) as archive:
    if archive.namelist() != ["deno"]:
        sys.exit("The Deno archive must hold only deno")
    archive.extract("deno", resources)
python = download(PYTHON_URL, PYTHON_SHA256)
run("tar", "-xzf", str(python), "-C", str(work))
# Only the interpreter, which links libpython statically, and its standard
# library. The app accepts only regular files: bin/python3 is the interpreter.
interpreter = (work / "python/bin/python3").resolve()
standard = interpreter.parent.parent / "lib" / interpreter.name
(resources / "python/bin").mkdir(parents=True)
shutil.copy(interpreter, resources / "python/bin/python3")
shutil.copytree(
    standard,
    resources / "python/lib" / standard.name,
    symlinks=True,
    ignore=shutil.ignore_patterns(*PYTHON_UNUSED),
)
for helper in ["yt-dlp", "deno", "python/bin/python3"]:
    (resources / helper).chmod(0o755)

for notice in [*root.glob("LICENSE*"), root / "NOTICE", root / "assets/fonts/OFL.txt"]:
    shutil.copy(notice, resources / "licenses")
for notice in ["LICENSE-DENO", "LICENSE-V8", "NOTICE-DENO", "LICENSE-YT-DLP"]:
    shutil.copy(root / "packaging" / notice, resources / "licenses")
shutil.copy(standard / "LICENSE.txt", resources / "licenses/LICENSE-PYTHON")

# The icon, inset to the macOS icon grid's 824 of 1024 pixels.
iconset = work / "AppIcon.iconset"
iconset.mkdir()
svg = (root / "packaging/yt-music-gpui.svg").read_text()
svg = svg.replace(
    'width="128" height="128" viewBox="0 0 128 128"',
    'width="1024" height="1024" viewBox="-5.5 -5.5 139 139"',
)
(work / "icon.svg").write_text(svg)
run("sips", "-s", "format", "png", str(work / "icon.svg"), "--out", str(iconset / "icon_512x512@2x.png"))
for size in [16, 32, 128, 256, 512]:
    for scale in [1, 2]:
        name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
        if name != "icon_512x512@2x.png":
            pixels = str(size * scale)
            run("sips", "-z", pixels, pixels, str(iconset / "icon_512x512@2x.png"), "--out", str(iconset / name))
run("iconutil", "-c", "icns", str(iconset), "-o", str(resources / "AppIcon.icns"))

# Nothing may load from Homebrew or anywhere else outside macOS and the app,
# and the app can start on the oldest macOS every bundled binary supports.
minimum = (11, 0)
for path in sorted(app.rglob("*")):
    if path.is_symlink() or not path.is_file() or not is_macho(path):
        continue
    for reference in linked(path):
        if reference.startswith("/") and not reference.startswith(SYSTEM):
            sys.exit(f"{path} still loads {reference}")
    for command in load_commands(path):
        rpath = command.get("path", "") if command.get("cmd") == "LC_RPATH" else ""
        if rpath.startswith("/") and not rpath.startswith(SYSTEM):
            sys.exit(f"{path} still searches {rpath}")
        field = {"LC_BUILD_VERSION": "minos", "LC_VERSION_MIN_MACOSX": "version"}.get(command.get("cmd"))
        if field:
            minimum = max(minimum, tuple(int(part) for part in command[field].split(".")))
minimum = ".".join(map(str, minimum))

with open(contents / "Info.plist", "wb") as file:
    plistlib.dump(
        {
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": APP_NAME,
            "CFBundleExecutable": "yt-music-gpui",
            "CFBundleIconFile": "AppIcon",
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": APP_NAME,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSApplicationCategoryType": "public.app-category.music",
            "LSMinimumSystemVersion": minimum,
            "NSHighResolutionCapable": True,
        },
        file,
    )

# install_name_tool invalidated the libraries' signatures. Deno and Python keep
# their own; the app's signature seals them as resources.
run("xattr", "-cr", str(app))
for library in sorted(frameworks.iterdir()):
    run("codesign", "--force", "--sign", "-", str(library))
run("codesign", "--force", "--sign", "-", str(app))
run("codesign", "--verify", "--deep", "--strict", str(app))

dmg = f"yt-music-gpui-{version}-aarch64-macos.dmg"
(dist / dmg).unlink(missing_ok=True)
(volume / "Applications").symlink_to("/Applications")
run(
    "hdiutil", "create", "-volname", APP_NAME, "-srcfolder", str(volume),
    "-fs", "HFS+", "-format", "ULMO", "-ov", str(dist / dmg),
)
digest = hashlib.sha256((dist / dmg).read_bytes()).hexdigest()
(dist / f"{dmg}.sha256").write_text(f"{digest}  {dmg}\n")
shutil.rmtree(work)
print(f"{dist / dmg}: {(dist / dmg).stat().st_size} bytes, macOS {minimum} or newer")
