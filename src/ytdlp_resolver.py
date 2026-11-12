"""Private, sequential --get-url server. All output stays in the parent's pipe."""

import contextlib
import io
import json
import re
import sys
import urllib.request

# A standalone yt-dlp zipapp carries its own package. For script installs this
# entry is ignored and the shebang's interpreter supplies the installed package.
sys.path.insert(0, sys.argv.pop(1))


def remember_player_scripts(ydl):
    """Keep YouTube's processed player script in memory between requests.

    Without it, every song that needs a JavaScript challenge re-processes the
    multi-megabyte player in a fresh JS runtime (~0.45 s). The cache lives only
    in this process, so nothing reaches the disk (--no-cache-dir still holds).
    Each processed player is ~4 MB, so only the most recent entries are kept
    when YouTube ships a new player mid-session.
    """
    from collections import OrderedDict

    from yt_dlp.cache import Cache
    try:
        from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP
    except ImportError:
        return  # Other yt-dlp versions keep their default behavior.

    class MemoryCache(Cache):
        enabled = True

        def __init__(self, ydl):
            super().__init__(ydl)
            self._entries = OrderedDict()

        def store(self, section, key, data, dtype='json'):
            self._entries[section, key] = data
            self._entries.move_to_end((section, key))
            while len(self._entries) > 8:
                self._entries.popitem(last=False)

        def load(self, section, key, dtype='json', default=None, *, min_ver=None):
            if (section, key) not in self._entries:
                return default
            self._entries.move_to_end((section, key))
            return self._entries[section, key]

    ydl.cache = MemoryCache(ydl)
    EJSBaseJCP._ENABLE_PREPROCESSED_PLAYER_CACHE = True


protocol = sys.stdout
output = io.StringIO()
with contextlib.redirect_stdout(output):
    import yt_dlp
    from yt_dlp.extractor.youtube import YoutubeIE
    from yt_dlp.globals import plugin_dirs
    from yt_dlp.plugins import load_all_plugins
    from yt_dlp.utils import std_headers

    class FastLogger:
        bot_check = False

        def debug(self, message):
            pass

        def warning(self, message):
            pass

        def error(self, message):
            self.bot_check |= 'not a bot' in message.lower()

    def visitor_data():
        # This opener has no cookie handler, including when the full lane is signed in.
        request = urllib.request.Request('https://music.youtube.com/', headers={
            'User-Agent': std_headers['User-Agent'],
        })
        with urllib.request.urlopen(request, timeout=10) as page:
            html = page.read().decode('utf-8')
        return json.loads('"' + re.search(r'"VISITOR_DATA"\s*:\s*"([^"\n]+)"', html)[1] + '"')

    def extract(ydl, request):
        output.seek(0)
        output.truncate()
        ydl.params['format'] = request['format']
        ydl.format_selector = ydl.build_format_selector(request['format'])
        # A failed download must not poison later requests on this instance.
        ydl._download_retcode = 0
        try:
            status = ydl.download([request['url']])
            urls = output.getvalue().splitlines()
            return urls if status == 0 and urls else None
        except Exception:
            # Extractor diagnostics can contain cookies and signed URLs.
            return None

    parsed = yt_dlp.parse_options(sys.argv[1:])
    # Match the CLI initialization order for the full lane only.
    plugin_dirs.value = parsed.options.plugin_dirs
    load_all_plugins()
    with contextlib.ExitStack() as instances:
        ydl = instances.enter_context(yt_dlp.YoutubeDL(parsed.ydl_opts))
        remember_player_scripts(ydl)
        if parsed.ydl_opts['allowed_extractors'] == ['^ytmusicaccount$']:
            if (set(ydl._ies) != {'YtMusicAccount'}
                    or ydl.get_info_extractor('YtMusicAccount').IE_NAME != 'ytmusicaccount'):
                raise RuntimeError('Account extractor is unavailable')
        fast = None
        logger = FastLogger()
        protocol.write('{"ready":true}\n')
        protocol.flush()
        for line in sys.stdin:
            try:
                request = json.loads(line)
                urls = None
                if request.get('lane') == 'fast':
                    try:
                        if fast is None:
                            # Never inherit the full instance's options, cookies, plugin
                            # registry or account identity. Register only the stock IE.
                            fast = instances.enter_context(yt_dlp.YoutubeDL({
                                'quiet': True, 'no_warnings': True, 'logger': logger,
                                'forceurl': True, 'simulate': True, 'noplaylist': True,
                                'cachedir': False,
                                'extractor_args': {'youtube': {
                                    'player_client': ['visionos'],
                                    'player_skip': ['webpage', 'initial_data', 'configs'],
                                    'skip': ['hls', 'dash'],
                                }},
                            }, auto_init=False))
                            fast.add_info_extractor(YoutubeIE())
                        args = fast.params['extractor_args']['youtube']
                        if 'visitor_data' not in args:
                            args['visitor_data'] = [visitor_data()]
                        logger.bot_check = False
                        urls = extract(fast, request)
                        if not urls and logger.bot_check:
                            args['visitor_data'] = [visitor_data()]
                            urls = extract(fast, request)
                    except Exception:
                        pass
                # Keep the URL payload unchanged. The optional flag identifies actual
                # fast-lane successes so mpv can retry an early open failure once.
                response = {'urls': urls, 'fast': True} if urls else {'urls': extract(ydl, request)}
            except Exception:
                response = {'urls': None}
            protocol.write(json.dumps(response) + '\n')
            protocol.flush()
