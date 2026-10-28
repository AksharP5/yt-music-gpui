from contextlib import closing
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
                cookies = helper["youtube_cookies"](temporary, Path(temporary) / "snapshot")
                self.assertEqual([cookie["name"] for cookie in cookies], ["SAPISID", "MUSIC", "WWW"])
                self.assertEqual(database.read_bytes(), before)
                self.assertEqual(writer.execute("SELECT count(*) FROM moz_cookies").fetchone()[0], len(rows))

    def test_absent_database_is_not_created(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(sqlite3.OperationalError):
                helper["youtube_cookies"](temporary, Path(temporary) / "snapshot")
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
                        helper["read_cookies"](database)
                    cookies = helper["youtube_cookies"](profile, snapshot)
                    self.assertEqual([(cookie["name"], cookie["value"], cookie["expires"]) for cookie in cookies],
                                     [("SAPISID", "latest-login", 4102444800), ("SESSION", "session", 0)])
                    self.assertEqual([Path(str(database) + suffix).read_bytes() for suffix in ("", "-wal")], before)
                    self.assertTrue(all(path.stat().st_mode & 0o077 == 0 for path in Path(snapshot).iterdir()))
                finally:
                    writer.communicate("q", timeout=5)

    def test_snapshot_rejects_oversized_database(self):
        with tempfile.TemporaryDirectory() as profile, tempfile.TemporaryDirectory() as snapshot:
            database = Path(profile) / "cookies.sqlite"
            with database.open("wb") as output:
                output.truncate(64 * 1024 * 1024 + 1)
            with self.assertRaisesRegex(ValueError, "too large"):
                helper["snapshot_database"](database, Path(snapshot))
            self.assertEqual(list(Path(snapshot).iterdir()), [])


unittest.main(argv=["browser_session_tests"])
