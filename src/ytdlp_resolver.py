"""Private, sequential --get-url server. All output stays in the parent's pipe."""

import contextlib
import io
import json
import sys

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
    from yt_dlp.globals import plugin_dirs
    from yt_dlp.plugins import load_all_plugins

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
    # Match the CLI initialization order.
    plugin_dirs.value = parsed.options.plugin_dirs
    load_all_plugins()
    with yt_dlp.YoutubeDL(parsed.ydl_opts) as ydl:
        remember_player_scripts(ydl)
        if parsed.ydl_opts['allowed_extractors'] == ['^ytmusicaccount$']:
            if (set(ydl._ies) != {'YtMusicAccount'}
                    or ydl.get_info_extractor('YtMusicAccount').IE_NAME != 'ytmusicaccount'):
                raise RuntimeError('Account extractor is unavailable')
        protocol.write('{"ready":true}\n')
        protocol.flush()
        for line in sys.stdin:
            try:
                request = json.loads(line)
                response = {'urls': extract(ydl, request)}
            except Exception:
                response = {'urls': None}
            protocol.write(json.dumps(response) + '\n')
            protocol.flush()
