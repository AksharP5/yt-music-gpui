"""Read the explicitly confirmed browser profile's YouTube session only."""

import configparser
from contextlib import closing, contextmanager
import ctypes
import functools
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

YOUTUBE_HOSTS = ('.youtube.com', 'youtube.com', '.music.youtube.com',
                 'music.youtube.com', '.www.youtube.com', 'www.youtube.com')
# Browser: (desktop entries, user data directory under XDG_CONFIG_HOME, Secret Service application).
CHROMIUM = {
    "Chromium": (("chromium.desktop", "chromium-browser.desktop"), "chromium", "chromium"),
    "Google Chrome": (("google-chrome.desktop",), "google-chrome", "chrome"),
    "Brave": (("brave-browser.desktop",), "BraveSoftware/Brave-Browser", "brave"),
}
UNAMBIGUOUS = "The browser has no unambiguous default profile. Set its default profile and retry."
UNDECRYPTABLE = "Could not decrypt the browser's YouTube session. Unlock your desktop keyring and retry."


def default_profile(desktop, home, config):
    if desktop in ("zen.desktop", "zen-browser.desktop", "app.zen_browser.zen.desktop"):
        return firefox_profile("Zen", (config / "zen", home / ".zen"))
    if desktop in ("firefox.desktop", "org.mozilla.firefox.desktop"):
        return firefox_profile("Firefox", (home / ".mozilla/firefox",))
    for browser, (desktops, directory, _) in CHROMIUM.items():
        if desktop in desktops:
            return chromium_profile(browser, config / directory)
    raise ValueError("Browser connection supports Zen, Firefox, Chromium, Google Chrome and Brave. "
                     "Your default browser is not supported.")


def chromium_profile(browser, root):
    try:
        state = json.loads((root / "Local State").read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise ValueError("Could not identify one browser profile directory. Open your browser once, then retry.") from None
    profiles = state.get("profile", {})
    # Links from other apps open in the profile that was used last.
    directory = profiles.get("last_used") or "Default"
    if not isinstance(directory, str) or directory in (".", "..") or "/" in directory:
        raise ValueError(UNAMBIGUOUS)
    path = root / directory
    if not path.is_dir():
        raise ValueError("The configured browser profile is unavailable. Open the browser and retry.")
    name = profiles.get("info_cache", {}).get(directory, {}).get("name") or directory
    return {"browser": browser, "name": str(name), "path": str(path.resolve())}


def firefox_profile(browser, roots):
    roots = [root for root in roots if (root / "profiles.ini").is_file()]
    if len(roots) != 1:
        raise ValueError("Could not identify one browser profile directory. Open your browser once, then retry.")
    root = roots[0]
    profiles = configparser.ConfigParser(interpolation=None)
    profiles.read(root / "profiles.ini", encoding="utf-8")
    installs = configparser.ConfigParser(interpolation=None)
    installs.read(root / "installs.ini", encoding="utf-8")
    defaults = {section["Default"] for section in installs.values() if "Default" in section}
    defaults.update(section["Default"] for key, section in profiles.items()
                    if key.startswith("Install") and "Default" in section)
    if len(defaults) > 1:
        raise ValueError(UNAMBIGUOUS)
    candidates = [section for key, section in profiles.items()
                  if key.startswith("Profile") and "Path" in section]
    if defaults:
        candidates = [section for section in candidates if section["Path"] in defaults]
    else:
        candidates = [section for section in candidates if section.get("Default") == "1"]
    if len(candidates) != 1:
        raise ValueError(UNAMBIGUOUS)
    profile = candidates[0]
    path = Path(profile["Path"])
    if profile.get("IsRelative") == "1":
        path = root / path
    if not path.is_absolute() or not path.is_dir():
        raise ValueError("The configured browser profile is unavailable. Open the browser and retry.")
    return {"browser": browser, "name": profile.get("Name", path.name), "path": str(path.resolve())}


@contextmanager
def read_transaction(database, mode):
    with closing(sqlite3.connect(database.as_uri() + "?mode=" + mode, uri=True, timeout=0.1)) as connection:
        connection.execute("PRAGMA temp_store=MEMORY")
        connection.execute("BEGIN")
        yield connection


def youtube_rows(connection, query, *parameters):
    rows = connection.execute(query.format(hosts=", ".join("?" * len(YOUTUBE_HOSTS))),
                              (*YOUTUBE_HOSTS, *parameters)).fetchall()
    if len(rows) > 256:
        raise ValueError("The browser returned too many YouTube cookies.")
    return rows


def read_firefox_cookies(database, mode="ro"):
    with read_transaction(database, mode) as connection:
        # Firefox schema 16 changed expiry from seconds to milliseconds.
        scale = 1000 if connection.execute("PRAGMA user_version").fetchone()[0] >= 16 else 1
        rows = youtube_rows(connection, """
            SELECT name, value, host, path, isSecure, expiry FROM moz_cookies
            WHERE host IN ({hosts}) AND originAttributes = '' AND path = '/' AND (expiry = 0 OR expiry > ?)
            ORDER BY creationTime LIMIT 257
        """, int(time.time() * scale))
    return [{"name": name, "value": value, "domain": domain, "path": path,
             "secure": bool(secure), "expires": expires // scale}
            for name, value, domain, path, secure, expires in rows]


def read_chromium_cookies(database, application, mode="ro"):
    # Chromium stores expiry in microseconds since 1601-01-01.
    epoch = 11644473600
    with read_transaction(database, mode) as connection:
        version = int(connection.execute("SELECT value FROM meta WHERE key = 'version'").fetchone()[0])
        rows = youtube_rows(connection, """
            SELECT name, value, encrypted_value, host_key, path, is_secure, expires_utc FROM cookies
            WHERE host_key IN ({hosts}) AND top_frame_site_key = '' AND path = '/'
              AND (expires_utc = 0 OR expires_utc > ?)
            ORDER BY creation_utc LIMIT 257
        """, (int(time.time()) + epoch) * 1_000_000)
    return [{"name": name, "value": decrypt_chromium_value(application, version, domain, value, encrypted),
             "domain": domain, "path": path, "secure": bool(secure),
             "expires": max(expires // 1_000_000 - epoch, 0)}
            for name, value, encrypted, domain, path, secure, expires in rows]


def decrypt_chromium_value(application, version, domain, value, encrypted):
    if not encrypted:
        return value
    prefix, ciphertext = encrypted[:3], encrypted[3:]
    if prefix not in (b"v10", b"v11"):
        raise ValueError("The browser encrypts cookies in an unsupported format.")
    plaintext = aes_128_cbc_decrypt(chromium_key(application, prefix), ciphertext)
    # Cookie database version 24 binds each value to its host with a SHA-256 prefix.
    if version >= 24:
        if plaintext[:32] != hashlib.sha256(domain.encode()).digest():
            raise ValueError(UNDECRYPTABLE)
        plaintext = plaintext[32:]
    try:
        return plaintext.decode("ascii")
    except UnicodeDecodeError:
        raise ValueError(UNDECRYPTABLE) from None


@functools.cache
def chromium_key(application, prefix):
    # v10 uses Chromium's fixed fallback password; v11 uses the Secret Service "Safe Storage" item.
    password = b"peanuts" if prefix == b"v10" else safe_storage_password(application)
    return hashlib.pbkdf2_hmac("sha1", password, b"saltysalt", 1, 16)


def safe_storage_password(application):
    result = subprocess.run(["/usr/bin/secret-tool", "lookup", "application", application],
                            stdin=subprocess.DEVNULL, capture_output=True, timeout=25)
    if result.returncode != 0 or not result.stdout:
        raise ValueError("Could not read the browser's Safe Storage key from your desktop keyring. "
                         "Unlock the keyring and retry.")
    return result.stdout


@functools.cache
def openssl():
    # The standard library has no AES, so use the system OpenSSL that Python itself links.
    crypto = ctypes.CDLL("libcrypto.so.3")
    crypto.EVP_CIPHER_CTX_new.restype = ctypes.c_void_p
    crypto.EVP_CIPHER_CTX_free.argtypes = (ctypes.c_void_p,)
    crypto.EVP_aes_128_cbc.restype = ctypes.c_void_p
    crypto.EVP_DecryptInit_ex.argtypes = (ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
                                          ctypes.c_char_p, ctypes.c_char_p)
    crypto.EVP_CIPHER_CTX_set_padding.argtypes = (ctypes.c_void_p, ctypes.c_int)
    crypto.EVP_DecryptUpdate.argtypes = (ctypes.c_void_p, ctypes.c_char_p, ctypes.POINTER(ctypes.c_int),
                                         ctypes.c_char_p, ctypes.c_int)
    return crypto


def aes_128_cbc_decrypt(key, ciphertext):
    if not ciphertext or len(ciphertext) % 16:
        raise ValueError(UNDECRYPTABLE)
    crypto = openssl()
    context = crypto.EVP_CIPHER_CTX_new()
    if not context:
        raise MemoryError
    output = ctypes.create_string_buffer(len(ciphertext))
    written = ctypes.c_int()
    try:
        # Chromium uses a fixed IV of 16 spaces and PKCS#7 padding, checked below.
        decrypted = (crypto.EVP_DecryptInit_ex(context, crypto.EVP_aes_128_cbc(), None, key, b" " * 16) == 1
                     and crypto.EVP_CIPHER_CTX_set_padding(context, 0) == 1
                     and crypto.EVP_DecryptUpdate(context, output, ctypes.byref(written),
                                                  ciphertext, len(ciphertext)) == 1)
    finally:
        crypto.EVP_CIPHER_CTX_free(context)
    plaintext = output.raw
    padding = plaintext[-1]
    if not decrypted or written.value != len(ciphertext) or not 1 <= padding <= 16 \
            or plaintext[-padding:] != bytes([padding]) * padding:
        raise ValueError(UNDECRYPTABLE)
    return plaintext[:-padding]


def snapshot_database(database, destination):
    paths = [Path(str(database) + suffix) for suffix in ("", "-wal", "-journal")]
    limit = 64 * 1024 * 1024

    def metadata():
        result = []
        for path in paths:
            try:
                stat = path.stat()
                result.append((stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns))
            except FileNotFoundError:
                result.append(None)
        return result

    def read_family(state):
        remaining = limit
        files = []
        for path, info in zip(paths, state):
            if info is None:
                files.append(None)
                continue
            with path.open("rb") as source:
                content = source.read(remaining + 1)
            remaining -= len(content)
            if remaining < 0:
                raise ValueError("The browser cookie database is too large to read safely.")
            files.append(content)
        return files

    # Zen holds an exclusive SQLite lock. Check the whole DB/WAL/journal family
    # twice before recovering a private copy; a DB-only copy can miss a new login.
    for _ in range(3):
        before = metadata()
        try:
            first = read_family(before)
            middle = metadata()
            second = read_family(middle)
        except FileNotFoundError:
            continue
        if before == middle == metadata() and first == second and first[0] is not None:
            for path, content in zip(paths, first):
                if content is not None:
                    with os.fdopen(os.open(destination / path.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as output:
                        output.write(content)
            return destination / database.name
        time.sleep(0.05)
    raise ValueError("The browser session changed while reading it. Finish signing in and retry.")


def youtube_cookies(browser, profile, snapshot):
    profile = Path(profile)
    if browser in ("Zen", "Firefox"):
        database, read = profile / "cookies.sqlite", read_firefox_cookies
    elif browser in CHROMIUM:
        # Chromium 96 moved the database into Network/; some builds still use the profile root.
        database = profile / "Network/Cookies"
        if not database.exists():
            database = profile / "Cookies"
        read = functools.partial(read_chromium_cookies, application=CHROMIUM[browser][2])
    else:
        raise ValueError("Unknown browser.")
    try:
        return read(database)
    except sqlite3.OperationalError as error:
        if error.sqlite_errorcode & 0xff not in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED):
            raise
    # SQLite rebuilds the WAL index and recovers a hot journal on this copy only.
    return read(snapshot_database(database, Path(snapshot)), mode="rw")


def main():
    try:
        mode, *arguments = sys.argv[1:]
        if mode == "profile":
            home = Path.home()
            result = default_profile(*arguments, home, Path(os.environ.get("XDG_CONFIG_HOME", home / ".config")))
        elif mode == "cookies":
            result = youtube_cookies(*arguments)
        else:
            raise ValueError("Unknown browser connection operation.")
        print(json.dumps(result))
    except (OSError, sqlite3.Error, configparser.Error, json.JSONDecodeError, subprocess.SubprocessError,
            AttributeError, TypeError):
        sys.exit("Could not read the browser session. Use a normal browser window, finish signing in, and retry.")
    except ValueError as error:
        sys.exit(str(error))


if __name__ == "__main__":
    main()
