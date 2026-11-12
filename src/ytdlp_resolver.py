"""Private, sequential --get-url server. All output stays in the parent's pipe."""

import contextlib
import io
import json
import re
import signal
import sys
import time
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


# A stalled fast lane gives up quickly and falls back to the full lane, leaving
# it most of the player's 30 s deadline. Socket timeouts alone cannot bound a
# response that trickles in, so the whole attempt also has one time budget.
FAST_TIMEOUT = 3
FAST_BUDGET = 5
# When YouTube keeps refusing the fast lane (bot checks on this network, an
# unreadable homepage), stop paying for it on every click for a while. A single
# stall is more likely a network blip, so it pauses the lane only briefly.
FAST_COOLDOWN = 600
FAST_STALL_COOLDOWN = 60


class FastBudgetExpired(BaseException):
    # Not an Exception, so yt-dlp's own handlers cannot swallow it.
    pass


def expire_fast_budget(signum, frame):
    raise FastBudgetExpired


protocol = sys.stdout
output = io.StringIO()
with contextlib.redirect_stdout(output):
    import yt_dlp
    from yt_dlp.globals import plugin_dirs
    from yt_dlp.plugins import load_all_plugins
    try:
        # Taken before plugins load, so an override plugin can never become the
        # fast lane's extractor. Without it, every request uses the full lane.
        from yt_dlp.extractor.youtube import YoutubeIE
    except Exception:
        YoutubeIE = None

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

    class FastLogger:
        bot_check = False

        def debug(self, message):
            pass

        def warning(self, message):
            pass

        def error(self, message):
            self.bot_check |= 'not a bot' in message.lower()

    class FastLane:
        def __init__(self, instances):
            self.instances = instances
            self.ydl = None
            self.logger = FastLogger()
            self.paused_until = 0

        def lookup(self, request):
            """Return stream URLs, or None to use the full lane. Never raises."""
            if YoutubeIE is None or time.monotonic() < self.paused_until:
                return None
            signal.signal(signal.SIGALRM, expire_fast_budget)
            signal.setitimer(signal.ITIMER_REAL, FAST_BUDGET)
            try:
                try:
                    return self.extract(request)
                finally:
                    signal.setitimer(signal.ITIMER_REAL, 0)
                    signal.signal(signal.SIGALRM, signal.SIG_IGN)
            except FastBudgetExpired:
                # Also covers the budget expiring as a lookup finishes.
                self.pause(FAST_STALL_COOLDOWN)
            except Exception:
                pass
            return None

        def pause(self, seconds=FAST_COOLDOWN):
            self.paused_until = time.monotonic() + seconds

        def extract(self, request):
            if self.ydl is None:
                # Never inherit the full instance's options, cookies, plugin
                # registry or account identity. Register only the stock IE.
                self.ydl = self.instances.enter_context(yt_dlp.YoutubeDL({
                    'quiet': True, 'no_warnings': True, 'logger': self.logger,
                    'forceurl': True, 'simulate': True, 'noplaylist': True,
                    'cachedir': False,
                    'socket_timeout': FAST_TIMEOUT, 'extractor_retries': 0,
                    'extractor_args': {'youtube': {
                        'player_client': ['visionos'],
                        'player_skip': ['webpage', 'initial_data', 'configs'],
                        'skip': ['hls', 'dash'],
                    }},
                }, auto_init=False))
                self.ydl.add_info_extractor(YoutubeIE())
            args = self.ydl.params['extractor_args']['youtube']
            if 'visitor_data' not in args:
                args['visitor_data'] = [self.visitor_data()]
            self.logger.bot_check = False
            urls = extract(self.ydl, request)
            if not urls and self.logger.bot_check:
                args['visitor_data'] = [self.visitor_data()]
                self.logger.bot_check = False
                urls = extract(self.ydl, request)
                if not urls and self.logger.bot_check:
                    self.pause()
            return urls

        def visitor_data(self):
            # This opener has no cookie handler, including when the full lane is signed in.
            request = urllib.request.Request('https://music.youtube.com/', headers={
                'User-Agent': self.ydl.params['http_headers']['User-Agent'],
            })
            try:
                with urllib.request.urlopen(request, timeout=FAST_TIMEOUT) as page:
                    html = page.read().decode('utf-8')
                found = re.search(r'"VISITOR_DATA"\s*:\s*"([^"\n]+)"', html)
                return json.loads('"' + found[1] + '"')
            except Exception:
                self.pause()
                raise

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
        fast = FastLane(instances)
        protocol.write('{"ready":true}\n')
        protocol.flush()
        for line in sys.stdin:
            try:
                request = json.loads(line)
                urls = fast.lookup(request) if request.get('lane') == 'fast' else None
                response = {'urls': urls or extract(ydl, request)}
            except Exception:
                response = {'urls': None}
            protocol.write(json.dumps(response) + '\n')
            protocol.flush()
