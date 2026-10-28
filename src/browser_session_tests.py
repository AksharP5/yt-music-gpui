import sqlite3
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
            with sqlite3.connect(database) as writer:
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
                cookies = helper["youtube_cookies"](temporary)
                self.assertEqual([cookie["name"] for cookie in cookies], ["SAPISID", "MUSIC", "WWW"])
                self.assertEqual(database.read_bytes(), before)
                self.assertEqual(writer.execute("SELECT count(*) FROM moz_cookies").fetchone()[0], len(rows))

    def test_absent_database_is_not_created(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(sqlite3.OperationalError):
                helper["youtube_cookies"](temporary)
            self.assertFalse((Path(temporary) / "cookies.sqlite").exists())


unittest.main(argv=["browser_session_tests"])
