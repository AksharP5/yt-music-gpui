from contextlib import closing
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

helper = {"__name__": "browser_session"}
exec(compile(sys.stdin.read(), "browser_session.py", "exec"), helper)


class BrowserSessionTests(unittest.TestCase):
    def test_install_default_wins_over_legacy_profile_default(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            root = home / ".config/zen"
            (root / "legacy").mkdir(parents=True)
            (root / "release").mkdir()
            (root / "profiles.ini").write_text("""
[Profile0]
Name=Legacy
Path=legacy
IsRelative=1
Default=1
[Profile1]
Name=Release profile
Path=release
IsRelative=1
[Install123]
Default=release
""")
            (root / "installs.ini").write_text("[123]\nDefault=release\n")
            profile = helper["default_profile"]("zen.desktop", home, home / ".config")
            self.assertEqual(profile, {"browser": "Zen", "name": "Release profile", "path": str(root / "release")})
            (root / "installs.ini").write_text("[123]\nDefault=release\n[456]\nDefault=legacy\n")
            with self.assertRaisesRegex(ValueError, "unambiguous"):
                helper["default_profile"]("zen.desktop", home, home / ".config")

    def test_live_wal_read_is_limited_to_normal_youtube_store(self):
        with tempfile.TemporaryDirectory() as temporary:
            database = Path(temporary) / "cookies.sqlite"
            with closing(sqlite3.connect(database)) as writer:
                writer.execute("PRAGMA journal_mode=WAL")
                writer.execute("PRAGMA wal_autocheckpoint=0")
                writer.execute("""CREATE TABLE moz_cookies (
                    name TEXT, value TEXT, host TEXT, path TEXT, isSecure INTEGER,
                    expiry INTEGER, originAttributes TEXT, creationTime INTEGER)""")
                writer.commit()
                rows = [
                    ("SAPISID", "synthetic", ".youtube.com", "/", 1, 4102444800, "", 1),
                    ("MUSIC", "synthetic", "music.youtube.com", "/", 1, 4102444800, "", 2),
                    ("WWW", "synthetic", "www.youtube.com", "/", 1, 4102444800, "", 3),
                    ("GOOGLE", "unrelated", ".google.com", "/", 1, 4102444800, "", 4),
                    ("LOOKALIKE", "unrelated", ".youtube.com.example", "/", 1, 4102444800, "", 5),
                    ("CONTAINER", "unrelated", ".youtube.com", "/", 1, 4102444800, "^userContextId=1", 6),
                    ("PARTITIONED", "unrelated", ".youtube.com", "/", 1, 4102444800, "^partitionKey=(https,other.example)", 7),
                    ("EXPIRED", "old", ".youtube.com", "/", 1, 1, "", 8),
                    ("OTHER_PATH", "unrelated", ".youtube.com", "/other", 1, 4102444800, "", 9),
                ]
                writer.executemany("INSERT INTO moz_cookies VALUES (?, ?, ?, ?, ?, ?, ?, ?)", rows)
                writer.commit()
                before = database.read_bytes()
                cookies = helper["youtube_cookies"]("Firefox", temporary, Path(temporary) / "snapshot")
                self.assertEqual([cookie["name"] for cookie in cookies], ["SAPISID", "MUSIC", "WWW"])
                self.assertEqual(database.read_bytes(), before)
                self.assertEqual(writer.execute("SELECT count(*) FROM moz_cookies").fetchone()[0], len(rows))

    def test_absent_database_is_not_created(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(sqlite3.OperationalError):
                helper["youtube_cookies"]("Zen", temporary, Path(temporary) / "snapshot")
            self.assertFalse((Path(temporary) / "cookies.sqlite").exists())

    def test_exclusive_wal_includes_latest_login_and_normalizes_modern_expiry(self):
        with tempfile.TemporaryDirectory() as profile, tempfile.TemporaryDirectory() as snapshot:
            database = Path(profile) / "cookies.sqlite"
            writer_code = """
import sqlite3, sys
connection = sqlite3.connect(sys.argv[1])
connection.execute('PRAGMA locking_mode=EXCLUSIVE')
connection.execute('PRAGMA journal_mode=WAL')
connection.execute('PRAGMA wal_autocheckpoint=0')
connection.execute('PRAGMA user_version=17')
connection.execute('CREATE TABLE moz_cookies (name TEXT, value TEXT, host TEXT, path TEXT, isSecure INTEGER, expiry INTEGER, originAttributes TEXT, creationTime INTEGER)')
connection.commit()
connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
connection.executemany('INSERT INTO moz_cookies VALUES (?, ?, ?, ?, ?, ?, ?, ?)', [
    ('SAPISID', 'latest-login', '.youtube.com', '/', 1, 4102444800000, '', 1),
    ('EXPIRED', 'old', '.youtube.com', '/', 1, 1600000000000, '', 2),
    ('SESSION', 'session', '.youtube.com', '/', 1, 0, '', 3),
    ('PRIVATE', 'other-site', '.google.com', '/', 1, 4102444800000, '', 4),
])
connection.commit()
print('ready', flush=True)
sys.stdin.read(1)
connection.close()
"""
            with subprocess.Popen([sys.executable, "-I", "-c", writer_code, str(database)],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as writer:
                try:
                    self.assertEqual(writer.stdout.readline().strip(), "ready")
                    self.assertFalse(Path(str(database) + "-shm").exists())
                    before = [Path(str(database) + suffix).read_bytes() for suffix in ("", "-wal")]
                    with self.assertRaisesRegex(sqlite3.OperationalError, "locked"):
                        helper["read_firefox_cookies"](database)
                    cookies = helper["youtube_cookies"]("Zen", profile, snapshot)
                    self.assertEqual([(cookie["name"], cookie["value"], cookie["expires"]) for cookie in cookies],
                                     [("SAPISID", "latest-login", 4102444800), ("SESSION", "session", 0)])
                    self.assertEqual([Path(str(database) + suffix).read_bytes() for suffix in ("", "-wal")], before)
                    self.assertTrue(all(path.stat().st_mode & 0o077 == 0 for path in Path(snapshot).iterdir()))
                finally:
                    writer.communicate("q", timeout=5)

    def test_chromium_default_profile_is_the_last_used_profile(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            root = home / ".config/chromium"
            (root / "Default").mkdir(parents=True)
            (root / "Profile 3").mkdir()
            state = {"profile": {"last_used": "Profile 3", "info_cache": {"Profile 3": {"name": "Music"}}}}
            (root / "Local State").write_text(json.dumps(state))
            profile = helper["default_profile"]("chromium.desktop", home, home / ".config")
            self.assertEqual(profile, {"browser": "Chromium", "name": "Music", "path": str(root / "Profile 3")})
            (root / "Local State").write_text("{}")
            profile = helper["default_profile"]("chromium.desktop", home, home / ".config")
            self.assertEqual(profile["path"], str(root / "Default"))
            (root / "Local State").write_text(json.dumps({"profile": {"last_used": "../chromium/Default"}}))
            with self.assertRaisesRegex(ValueError, "unambiguous"):
                helper["default_profile"]("chromium.desktop", home, home / ".config")
            with self.assertRaisesRegex(ValueError, "Open your browser once"):
                helper["default_profile"]("google-chrome.desktop", home, home / ".config")

    def chromium_profile(self, directory, version, rows):
        database = Path(directory) / "Network/Cookies"
        database.parent.mkdir()
        with closing(sqlite3.connect(database)) as writer:
            writer.execute("CREATE TABLE meta (key TEXT, value TEXT)")
            writer.execute("INSERT INTO meta VALUES ('version', ?)", (str(version),))
            writer.execute("""CREATE TABLE cookies (
                creation_utc INTEGER, host_key TEXT, top_frame_site_key TEXT, name TEXT, value TEXT,
                encrypted_value BLOB, path TEXT, expires_utc INTEGER, is_secure INTEGER)""")
            writer.executemany("INSERT INTO cookies VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                               [(index, host, partition, name, "", encrypted, path, expires, 1)
                                for index, (host, partition, name, encrypted, path, expires) in enumerate(rows)])
            writer.commit()
        return database

    def keyring(self, password):
        lookups = []
        helper["chromium_key"].cache_clear()
        self.addCleanup(helper["chromium_key"].cache_clear)
        original = helper["safe_storage_password"]
        self.addCleanup(helper.__setitem__, "safe_storage_password", original)
        helper["safe_storage_password"] = lambda application: lookups.append(application) or password
        return lookups

    def test_chromium_cookies_decrypt_v11_keyring_and_v10_fallback_values(self):
        # Encrypted with `openssl enc -aes-128-cbc` using Chromium's key derivation and IV.
        # v11: keyring password "synthetic-safe-storage"; v10: Chromium's fixed password.
        # Each plaintext starts with SHA-256 of its host, as in cookie database version 24.
        keyring_value = b"v11" + bytes.fromhex(
            "17f911b7385fa70e4798db5cb6372e68ddbc329eed3cb9bd5f65b6e36d08947f8a1b6ce029fd87b07591b838f9cafe67")
        fallback_value = b"v10" + bytes.fromhex(
            "aa98ae9044d479a531b64b16dbc66229d78cd05065b4f0c14fabc62f4fc86b5a"
            "5b8e45f65cb36c23ed7732b278a11b8a6a0da3bfabd7d3a4843ac00608c7bfd3")
        future = (4102444800 + 11644473600) * 1_000_000
        with tempfile.TemporaryDirectory() as profile:
            database = self.chromium_profile(profile, 24, [
                (".youtube.com", "", "SAPISID", keyring_value, "/", future),
                ("music.youtube.com", "", "MUSIC", fallback_value, "/", 0),
                (".youtube.com", "https://other.example", "PARTITIONED", keyring_value, "/", future),
                (".youtube.com", "", "EXPIRED", keyring_value, "/", 1),
                (".youtube.com", "", "OTHER_PATH", keyring_value, "/other", future),
                (".google.com", "", "GOOGLE", keyring_value, "/", future),
            ])
            lookups = self.keyring(b"synthetic-safe-storage")
            before = database.read_bytes()
            cookies = helper["youtube_cookies"]("Google Chrome", profile, Path(profile) / "snapshot")
            self.assertEqual(cookies, [
                {"name": "SAPISID", "value": "keyring-sapisid", "domain": ".youtube.com", "path": "/",
                 "secure": True, "expires": 4102444800},
                {"name": "MUSIC", "value": "basic-store-music", "domain": "music.youtube.com", "path": "/",
                 "secure": True, "expires": 0},
            ])
            self.assertEqual(lookups, ["chrome"])
            self.assertEqual(database.read_bytes(), before)
            # A running Chromium holds an exclusive lock, so the reader falls back to a private snapshot.
            with closing(sqlite3.connect(database)) as browser:
                browser.execute("PRAGMA locking_mode=EXCLUSIVE")
                browser.execute("UPDATE meta SET value = value")
                browser.commit()
                with self.assertRaisesRegex(sqlite3.OperationalError, "locked"):
                    helper["read_chromium_cookies"](database, "chrome")
                snapshot = Path(profile) / "snapshot"
                snapshot.mkdir()
                self.assertEqual(helper["youtube_cookies"]("Google Chrome", profile, snapshot), cookies)

    def test_chromium_values_bound_to_another_host_or_key_are_rejected(self):
        keyring_value = b"v11" + bytes.fromhex(
            "17f911b7385fa70e4798db5cb6372e68ddbc329eed3cb9bd5f65b6e36d08947f8a1b6ce029fd87b07591b838f9cafe67")
        for host, password in [("music.youtube.com", b"synthetic-safe-storage"), (".youtube.com", b"wrong")]:
            with self.subTest(host=host, password=password), tempfile.TemporaryDirectory() as profile:
                self.chromium_profile(profile, 24, [(host, "", "SAPISID", keyring_value, "/", 0)])
                self.keyring(password)
                with self.assertRaisesRegex(ValueError, "decrypt"):
                    helper["youtube_cookies"]("Chromium", profile, Path(profile) / "snapshot")

    def test_chromium_cookies_before_version_24_have_no_host_prefix(self):
        legacy_value = b"v11" + bytes.fromhex("e4b28f92b6254c10848c12f6856fd5fa")
        with tempfile.TemporaryDirectory() as profile:
            self.chromium_profile(profile, 23, [(".youtube.com", "", "SAPISID", legacy_value, "/", 0)])
            lookups = self.keyring(b"synthetic-safe-storage")
            cookies = helper["youtube_cookies"]("Brave", profile, Path(profile) / "snapshot")
            self.assertEqual([(cookie["name"], cookie["value"]) for cookie in cookies], [("SAPISID", "legacy-sapisid")])
            self.assertEqual(lookups, ["brave"])

    def test_snapshot_rejects_oversized_database(self):
        with tempfile.TemporaryDirectory() as profile, tempfile.TemporaryDirectory() as snapshot:
            database = Path(profile) / "cookies.sqlite"
            with database.open("wb") as output:
                output.truncate(64 * 1024 * 1024 + 1)
            with self.assertRaisesRegex(ValueError, "too large"):
                helper["snapshot_database"](database, Path(snapshot))
            self.assertEqual(list(Path(snapshot).iterdir()), [])


unittest.main(argv=["browser_session_tests"])
