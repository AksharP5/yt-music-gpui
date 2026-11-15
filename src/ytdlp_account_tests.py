"""Offline contract tests for the embedded extractor and installed yt-dlp."""

import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import subprocess
import unittest
import urllib.error
import urllib.request
import urllib.response
from email.message import Message
from unittest.mock import patch

zipapp = os.environ.get('YT_DLP_TEST_ZIPAPP')
if zipapp:
    sys.path.insert(0, zipapp)

import yt_dlp
from yt_dlp.extractor.youtube import YoutubeIE
from yt_dlp.globals import plugin_dirs
from yt_dlp.networking import Request
from yt_dlp.plugins import load_all_plugins
from yt_dlp.networking.exceptions import HTTPError
from yt_dlp.utils import DownloadError, ExtractorError

plugin_dirs.value = []
plugin_root = Path(sys.argv.pop(1))
server_script = Path(sys.argv.pop(1)).read_text()
spec = importlib.util.spec_from_file_location("account_contract", plugin_root / "account/yt_dlp_plugins/extractor/account.py")
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)


COOKIES = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tINVENTED_COOKIE\n"


def credentials(account):
    """The line the app writes to the extractor's stdin."""
    return json.dumps({"cookies": COOKIES, "account": account}) + "\n"


class Probe(plugin.YtMusicAccountIE):
    def _get_sid_authorization_header(self, origin=None, user_session_id=None):
        return "INVENTED_AUTH_FOR_" + str(user_session_id)

    def _extract_response(self, **kwargs):
        self.response_args = kwargs
        return getattr(self, "synthetic_response", {"synthetic": True})


class AccountContract(unittest.TestCase):
    def extractor(self, delegated=None, *, stdin=None):
        account = {"authuser": "2", "delegated_session_id": delegated,
                   "data_sync_id": f"{delegated}||user" if delegated else "user||"}
        ydl = yt_dlp.YoutubeDL({"quiet": True}, auto_init=False)
        ydl.urlopen = lambda *_: self.fail("an offline test attempted a network request")
        ie = Probe(ydl)
        with patch.object(sys, "stdin", io.StringIO(credentials(account) if stdin is None else stdin)):
            try:
                ie._account
            except DownloadError:
                pass
        return ie

    def test_nonzero_account_and_brand_reach_headers_and_context(self):
        for brand in [None, "brand"]:
            with self.subTest(brand=brand):
                ie = self.extractor(brand)
                ie._extract_player_response("web", "abcdefghijk", {}, {}, None, {}, None,
                                           ie._extract_data_sync_id(), None)
                headers = ie.response_args["headers"]
                self.assertEqual(headers["X-Goog-AuthUser"], 2)
                self.assertEqual(headers.get("X-Goog-PageId"), brand)
                self.assertEqual(headers["Authorization"], "INVENTED_AUTH_FOR_user")
                context = ie._extract_context({"INNERTUBE_CONTEXT": {
                    "client": {}, "user": {"onBehalfOfUser": "other-brand"}}})
                self.assertEqual(context["user"], {"onBehalfOfUser": brand} if brand else {})

    def test_downloaded_account_mismatches_are_rejected_without_disclosure(self):
        ie = self.extractor("brand")
        for field, value in [("SESSION_INDEX", 0), ("DELEGATED_SESSION_ID", "other-brand"),
                             ("USER_SESSION_ID", "other-user"), ("DATASYNC_ID", "other||account")]:
            html = "ytcfg.set(" + json.dumps({field: value}) + ");"
            with self.subTest(field=field), self.assertRaises(DownloadError) as caught:
                ie.extract_ytcfg("abcdefghijk", html)
            self.assertNotIn(str(value), str(caught.exception))
        with self.assertRaises(DownloadError):
            ie._extract_player_response("web", "abcdefghijk", {"SESSION_INDEX": 0},
                                       {}, None, {}, None, "brand||user", None)
        with self.assertRaises(DownloadError):
            ie._extract_data_sync_id({"responseContext": {
                "mainAppWebResponseContext": {"datasyncId": "other||account"}}})

    def test_every_youtube_request_has_the_bound_identity(self):
        for brand in [None, "brand"]:
            ie = self.extractor(brand)
            ie._set_cookie(".youtube.com", "SAPISID", "INVENTED_COOKIE", secure=True)
            ie._set_cookie(".youtube.com", "LOGIN_INFO", "INVENTED_LOGIN", secure=True)
            with patch.object(YoutubeIE, "_request_webpage", return_value=None) as parent:
                ie._request_webpage(Request("https://www.youtube.com/watch?v=abcdefghijk", headers={
                    "X-Goog-AuthUser": "0", "X-Goog-PageId": "other-brand"}), "abcdefghijk")
            headers = parent.call_args.args[0].headers
            self.assertEqual(str(headers["X-Goog-AuthUser"]), "2")
            self.assertEqual(headers.get("X-Goog-PageId"), brand)
            self.assertEqual(headers["Authorization"], "INVENTED_AUTH_FOR_user")

    def test_player_and_next_response_mismatches_are_fatal(self):
        responses = [
            {"responseContext": {"mainAppWebResponseContext": {"datasyncId": "other||account"}}},
            {"responseContext": {"mainAppWebResponseContext": {"loggedOut": True}}},
            {"LOGGED_IN": False},
        ]
        ie = self.extractor("brand")
        for response in responses:
            ie.synthetic_response = response
            with self.assertRaises(DownloadError) as caught:
                ie._extract_player_response("web", "abcdefghijk", {}, {}, None, {}, None, "brand||user", None)
            self.assertNotIsInstance(caught.exception, ExtractorError)
            with patch.object(YoutubeIE, "_extract_response", return_value=response):
                with self.assertRaises(DownloadError):
                    plugin.YtMusicAccountIE._extract_response(ie, item_id="abcdefghijk", query={}, ep="next")

    def test_formats_require_identity_confirmed_by_downloaded_data(self):
        ie = self.extractor("brand")
        formats = {"formats": [{"url": "https://media.invalid/synthetic"}]}
        # Our own request hints are not server confirmation.
        ie.generate_api_headers(ytcfg={"DATASYNC_ID": "brand||user"})
        with patch.object(YoutubeIE, "_real_extract", return_value=formats):
            with self.assertRaises(DownloadError):
                ie._real_extract("https://music.youtube.com/watch?v=abcdefghijk")

        def confirmed_extract(ie, url):
            ie.extract_ytcfg("abcdefghijk", 'ytcfg.set({"DATASYNC_ID":"brand||user"});')
            return formats

        with patch.object(YoutubeIE, "_real_extract", new=confirmed_extract):
            self.assertEqual(ie._real_extract("https://music.youtube.com/watch?v=abcdefghijk"), formats)
        ie._server_identity_confirmed = False
        with patch.object(YoutubeIE, "_extract_player_responses", return_value=([
            {"responseContext": {"mainAppWebResponseContext": {"datasyncId": "brand||user"}}}], None)):
            ie._extract_player_responses([], "abcdefghijk", None, "web", {}, False)
        self.assertTrue(ie._server_identity_confirmed)

    def test_authenticated_transport_never_dispatches_a_redirect(self):
        requests = []

        class FakeHTTPS(urllib.request.HTTPSHandler):
            handler_order = 0

            def https_open(self, request):
                requests.append(request.full_url)
                headers = Message()
                headers["Location"] = "https://different.invalid/receive"
                response = urllib.response.addinfourl(io.BytesIO(), headers, request.full_url, 302)
                response.msg = "Found"
                return response

        class OfflineRH(plugin._AccountUrllibRH):
            def _create_instance(self, *args, **kwargs):
                opener = super()._create_instance(*args, **kwargs)
                opener.add_handler(FakeHTTPS())
                return opener

        ydl = self.extractor()._downloader
        director = ydl.build_request_director([OfflineRH])
        try:
            with self.assertRaises(HTTPError):
                director.send(Request("https://www.youtube.com/youtubei/v1/player", headers={
                    "Authorization": "INVENTED_AUTH", "X-Goog-AuthUser": "2"}))
        finally:
            director.close()
        self.assertEqual(requests, ["https://www.youtube.com/youtubei/v1/player"])

    def test_credentials_load_into_memory_only(self):
        ie = self.extractor("brand")
        self.assertEqual(ie._account["data_sync_id"], "brand||user")
        self.assertEqual([cookie.value for cookie in ie.cookiejar], ["INVENTED_COOKIE"])
        self.assertIsNone(ie._downloader.params.get("cookiefile"))

    def test_missing_or_malformed_credentials_fail_before_network(self):
        account = {"authuser": "2", "delegated_session_id": None, "data_sync_id": "user||"}
        for stdin in ["", credentials(account)[:-1], "{}\n", "[]\n", "not json\n",
                      json.dumps({"cookies": COOKIES, "account": account, "extra": 1}) + "\n",
                      json.dumps({"cookies": None, "account": account}) + "\n",
                      credentials({**account, "authuser": "02x"}),
                      credentials({**account, "delegated_session_id": "other"}),
                      json.dumps({"cookies": "x" * (300 * 1024), "account": account}) + "\n"]:
            # A failed read is not cached; later calls must find nothing either.
            with self.subTest(stdin=stdin[:40]), patch.object(sys, "stdin", io.StringIO()):
                ie = self.extractor(stdin=stdin)
                with self.assertRaises(DownloadError):
                    ie._extract_session_index({})
                with self.assertRaises(DownloadError):
                    ie._request_webpage("https://www.youtube.com", "abcdefghijk")
                self.assertEqual(list(ie.cookiejar), [])
        with patch.object(plugin, "__version__", "unreviewed"):
            with self.assertRaises(DownloadError):
                self.extractor()._extract_session_index({})

    def test_disabled_plugin_cannot_fall_back_to_stock_extraction(self):
        code = """
import sys
from yt_dlp import YoutubeDL
from yt_dlp.globals import plugin_dirs
from yt_dlp.plugins import load_all_plugins
from yt_dlp.utils import DownloadError
plugin_dirs.value = [sys.argv[1]]
load_all_plugins()
with YoutubeDL({'quiet': True, 'allowed_extractors': ['^ytmusicaccount$']}) as ydl:
    assert not ydl._ies
    def no_network(*args, **kwargs):
        raise AssertionError('stock extraction attempted network access')
    ydl.urlopen = no_network
    try:
        ydl.extract_info('https://music.youtube.com/watch?v=abcdefghijk', download=False)
    except DownloadError:
        pass
    else:
        raise AssertionError('missing plugin did not fail closed')
"""
        result = subprocess.run([sys.executable, "-B", "-c", code, str(plugin_root)],
                                env={**os.environ, "YTDLP_NO_PLUGINS": "1"}, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())

    def test_account_arguments_load_only_the_account_extractor(self):
        _, opts, _, params = yt_dlp.parse_options([
            "--ignore-config", "--no-plugin-dirs", "--plugin-dirs", str(plugin_root),
            "--use-extractors", "^ytmusicaccount$"])
        self.assertEqual(opts.plugin_dirs, [str(plugin_root)])
        plugin_dirs.value = opts.plugin_dirs
        load_all_plugins()
        with yt_dlp.YoutubeDL(params) as ydl:
            self.assertEqual([ie.IE_NAME for ie in ydl._ies.values()], ["ytmusicaccount"])


class WarmServerContract(unittest.TestCase):
    ACCOUNT = credentials({"authuser": "2", "delegated_session_id": "brand", "data_sync_id": "brand||user"})

    def server(self, *, disabled=False, one_shot=False):
        # Patch the remote boundary only. The real plugin still selects authority,
        # verifies each response, and must be discovered by the server's CLI setup.
        code = r"""
import sys
zipapp = sys.argv.pop(1)
if zipapp:
    sys.path.insert(0, zipapp)
import yt_dlp
from yt_dlp.extractor.youtube import YoutubeIE
from yt_dlp.utils import DownloadError

def no_network(*args, **kwargs):
    raise AssertionError('offline contract attempted a network request')

yt_dlp.YoutubeDL.urlopen = no_network
YoutubeIE._real_initialize = lambda self: None
# Only the first request sees a player response, so loudness must not carry over.
YoutubeIE._extract_player_responses = lambda self, *args, **kwargs: ([{
    'streamingData': {'adaptiveFormats': [{'itag': 251, 'loudnessDb': 2.5}]},
    'playerConfig': {'audioConfig': {'loudnessDb': float('nan'), 'loudnessTargetLkfs': -14}},
}], None)

def extract(self, url):
    self.calls = getattr(self, 'calls', 0) + 1
    assert self.IE_NAME == 'ytmusicaccount'
    assert self._account['authuser'] == 2
    assert self._account['data_sync_id'] == 'brand||user'
    assert any(cookie.value == 'INVENTED_COOKIE' for cookie in self.cookiejar)
    assert not self._server_identity_confirmed, 'identity confirmation leaked across requests'
    if url.endswith('badbadbad00'):
        self._verify_server_identity({'DATASYNC_ID': 'other||account'})
    if not url.endswith('unconfirmed'):
        self._verify_server_identity({'DATASYNC_ID': 'brand||user'})
    if self.calls == 1:
        self._extract_player_responses()
    return {'id': 'abcdefghijk', 'title': 'Synthetic', 'formats': [
        {'format_id':'251', 'url':f'https://rr1.googlevideo.com/audio?itag=251&call={self.calls}',
         'ext':'webm', 'vcodec':'none', 'acodec':'opus'},
        {'format_id':'video', 'url':'https://rr1.googlevideo.com/video',
         'ext':'webm', 'vcodec':'vp9', 'acodec':'none', 'height':720}]}

YoutubeIE._real_extract = extract
if sys.argv[1] == 'one-shot':
    del sys.argv[1:3]
    sys.exit(yt_dlp.main(sys.argv[1:]))
exec(sys.argv.pop(1))
"""
        server = subprocess.Popen([sys.executable, '-I', '-B', '-u', '-c', code,
                                   zipapp or '', 'one-shot' if one_shot else server_script, zipapp or 'unused-script',
                                   '--ignore-config', '--get-url', '--no-cache-dir', '--quiet', '--no-warnings',
                                   '--no-playlist', '--no-plugin-dirs', '--plugin-dirs', str(plugin_root),
                                   '--use-extractors', '^ytmusicaccount$',
                                   *(['--format', 'bestaudio', 'https://music.youtube.com/watch?v=abcdefghijk'] if one_shot else [])],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  text=True, env={**os.environ, **({'YTDLP_NO_PLUGINS': '1'} if disabled else {})})
        def cleanup():
            if server.poll() is None:
                server.kill()
            server.wait()
            for pipe in [server.stdin, server.stdout, server.stderr]:
                pipe.close()
        self.addCleanup(cleanup)
        return server

    def test_warm_server_uses_account_plugin_and_reuses_it_for_audio_and_video(self):
        server = self.server()
        requests = [
            {'url': 'https://music.youtube.com/watch?v=abcdefghijk', 'format': 'bestaudio'},
            {'url': 'https://music.youtube.com/watch?v=badbadbad00', 'format': 'bestaudio'},
            {'url': 'https://music.youtube.com/watch?v=unconfirmed', 'format': 'bestaudio'},
            {'url': 'https://music.youtube.com/watch?v=abcdefghijk', 'format': 'bestvideo+bestaudio'},
        ]
        stdout, stderr = server.communicate(self.ACCOUNT + ''.join(json.dumps(request) + '\n' for request in requests), timeout=15)
        self.assertEqual(server.returncode, 0, stderr)
        self.assertEqual([json.loads(line) for line in stdout.splitlines()], [
            {'ready': True},
            {'urls': ['https://rr1.googlevideo.com/audio?itag=251&call=1'],
             'loudness': {'audioConfig': {'loudnessTargetLkfs': -14.0}, 'format': {'loudnessDb': 2.5}}},
            {'urls': None}, {'urls': None},
            {'urls': ['https://rr1.googlevideo.com/video', 'https://rr1.googlevideo.com/audio?itag=251&call=4'],
             'loudness': None},
        ])
        self.assertNotIn('INVENTED_COOKIE', stdout)
        self.assertNotIn('other||account', stdout)

    def test_warm_server_refuses_startup_without_the_account_plugin(self):
        server = self.server(disabled=True)
        stdout, _ = server.communicate(self.ACCOUNT, timeout=15)
        self.assertNotEqual(server.returncode, 0)
        self.assertEqual(stdout, '')

    def test_warm_server_refuses_startup_without_credentials(self):
        server = self.server()
        request = json.dumps({'url': 'https://music.youtube.com/watch?v=abcdefghijk', 'format': 'bestaudio'})
        stdout, _ = server.communicate(request + '\n', timeout=15)
        self.assertNotEqual(server.returncode, 0)
        self.assertEqual(stdout, '')

    def test_one_shot_cli_reads_the_same_credentials(self):
        server = self.server(one_shot=True)
        stdout, stderr = server.communicate(self.ACCOUNT, timeout=15)
        self.assertEqual(server.returncode, 0, stderr)
        self.assertEqual(stdout, 'https://rr1.googlevideo.com/audio?itag=251&call=1\n')


unittest.main()
