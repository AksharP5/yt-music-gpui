"""Read the explicitly confirmed browser profile's YouTube session only."""

import configparser
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


def youtube_cookies(profile):
    database = Path(profile) / "cookies.sqlite"
    # mode=ro includes committed WAL changes without copying other sites' data.
    # Containers and partitioned stores are intentionally excluded.
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=2) as connection:
        rows = connection.execute("""
            SELECT name, value, host, path, isSecure, expiry FROM moz_cookies
            WHERE host IN ('.youtube.com', 'youtube.com', '.music.youtube.com',
                           'music.youtube.com', '.www.youtube.com', 'www.youtube.com')
              AND originAttributes = '' AND path = '/' AND (expiry = 0 OR expiry > ?)
            ORDER BY creationTime
            LIMIT 257
        """, (int(time.time()),)).fetchall()
    if len(rows) > 256:
        raise ValueError("The browser returned too many YouTube cookies.")
    return [{"name": name, "value": value, "domain": domain, "path": path,
             "secure": bool(secure), "expires": expires}
            for name, value, domain, path, secure, expires in rows]


def main():
    try:
        mode, value = sys.argv[1:]
        if mode == "profile":
            home = Path.home()
            result = default_profile(value, home, Path(os.environ.get("XDG_CONFIG_HOME", home / ".config")))
        elif mode == "cookies":
            result = youtube_cookies(value)
        else:
            raise ValueError("Unknown browser connection operation.")
        print(json.dumps(result))
    except (OSError, sqlite3.Error, configparser.Error):
        sys.exit("Could not read the browser session. Use a normal browser window, finish signing in, and retry.")
    except ValueError as error:
        sys.exit(str(error))


if __name__ == "__main__":
    main()
