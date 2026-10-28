"""Read the explicitly confirmed browser profile's YouTube session only."""

import configparser
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import sys
import time


def default_profile(desktop, home, config):
    if desktop in ("zen.desktop", "zen-browser.desktop", "app.zen_browser.zen.desktop"):
        browser, roots = "Zen", (config / "zen", home / ".zen")
    elif desktop in ("firefox.desktop", "org.mozilla.firefox.desktop"):
        browser, roots = "Firefox", (home / ".mozilla/firefox",)
    else:
        raise ValueError("Browser connection currently supports Zen and Firefox. Your default browser is not supported.")
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
        raise ValueError("The browser has no unambiguous default profile. Set its default profile and retry.")
    candidates = [section for key, section in profiles.items()
                  if key.startswith("Profile") and "Path" in section]
    if defaults:
        candidates = [section for section in candidates if section["Path"] in defaults]
    else:
        candidates = [section for section in candidates if section.get("Default") == "1"]
    if len(candidates) != 1:
        raise ValueError("The browser has no unambiguous default profile. Set its default profile and retry.")
    profile = candidates[0]
    path = Path(profile["Path"])
    if profile.get("IsRelative") == "1":
        path = root / path
    if not path.is_absolute() or not path.is_dir():
        raise ValueError("The configured browser profile is unavailable. Open the browser and retry.")
    return {"browser": browser, "name": profile.get("Name", path.name), "path": str(path.resolve())}


def read_cookies(database, mode="ro"):
    with closing(sqlite3.connect(database.as_uri() + "?mode=" + mode, uri=True, timeout=0.1)) as connection:
        connection.execute("PRAGMA temp_store=MEMORY")
        connection.execute("BEGIN")
        # Firefox schema 16 changed expiry from seconds to milliseconds.
        scale = 1000 if connection.execute("PRAGMA user_version").fetchone()[0] >= 16 else 1
        rows = connection.execute("""
            SELECT name, value, host, path, isSecure, expiry FROM moz_cookies
            WHERE host IN ('.youtube.com', 'youtube.com', '.music.youtube.com',
                           'music.youtube.com', '.www.youtube.com', 'www.youtube.com')
              AND originAttributes = '' AND path = '/' AND (expiry = 0 OR expiry > ?)
            ORDER BY creationTime
            LIMIT 257
        """, (int(time.time() * scale),)).fetchall()
    if len(rows) > 256:
        raise ValueError("The browser returned too many YouTube cookies.")
    return [{"name": name, "value": value, "domain": domain, "path": path,
             "secure": bool(secure), "expires": expires // scale}
            for name, value, domain, path, secure, expires in rows]


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


def youtube_cookies(profile, snapshot):
    database = Path(profile) / "cookies.sqlite"
    try:
        return read_cookies(database)
    except sqlite3.OperationalError as error:
        if error.sqlite_errorcode & 0xff not in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED):
            raise
    # SQLite rebuilds the WAL index and recovers a hot journal on this copy only.
    return read_cookies(snapshot_database(database, Path(snapshot)), mode="rw")


def main():
    try:
        mode, value, *snapshot = sys.argv[1:]
        if mode == "profile":
            home = Path.home()
            result = default_profile(value, home, Path(os.environ.get("XDG_CONFIG_HOME", home / ".config")))
        elif mode == "cookies":
            result = youtube_cookies(value, snapshot[0])
        else:
            raise ValueError("Unknown browser connection operation.")
        print(json.dumps(result))
    except (OSError, sqlite3.Error, configparser.Error):
        sys.exit("Could not read the browser session. Use a normal browser window, finish signing in, and retry.")
    except ValueError as error:
        sys.exit(str(error))


if __name__ == "__main__":
    main()
