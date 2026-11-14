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


def report_loudness(ydl):
    """Return a function that reports the loudness YouTube sent for the last
    request's selected audio format, so the player can level volume the way
    YouTube Music does. yt-dlp drops these player response fields.

    The returned function also forgets the request, and returns None when the
    fields are missing or this yt-dlp version extracts differently.
    """
    from urllib.parse import parse_qs, urlsplit

    from yt_dlp.extractor.youtube import YoutubeIE

    responses, selected = [], []
    extract_player_responses = YoutubeIE._extract_player_responses
    process_info = ydl.process_info

    def remember_responses(self, *args, **kwargs):
        result = extract_player_responses(self, *args, **kwargs)
        responses[:] = result[0]
        return result

    def remember_selection(info):
        selected[:] = [info]
        return process_info(info)

    YoutubeIE._extract_player_responses = remember_responses
    ydl.process_info = remember_selection

    def pick(fields, names):
        return {name: fields[name] for name in names if isinstance(fields.get(name), (int, float))}

    def loudness():
        try:
            info = selected[0]
            audio = next(f for f in info.get('requested_formats') or [info] if f.get('acodec') != 'none')
            itag = parse_qs(urlsplit(audio['url']).query)['itag'][0]
            drc = 'drc' in audio['format_id'].split('-')
            for response in responses:
                data = response.get('streamingData') or {}
                for raw in (data.get('formats') or []) + (data.get('adaptiveFormats') or []):
                    if str(raw.get('itag')) == itag and bool(raw.get('isDrc')) == drc:
                        config = (response.get('playerConfig') or {}).get('audioConfig') or {}
                        return {
                            'audioConfig': pick(config, ('loudnessDb', 'trackAbsoluteLoudnessLkfs', 'loudnessTargetLkfs')),
                            'format': pick(raw, ('loudnessDb', 'trackAbsoluteLoudnessLkfs')),
                        }
        except Exception:
            pass
        finally:
            responses.clear()
            selected.clear()
        return None

    return loudness


protocol = sys.stdout
output = io.StringIO()
with contextlib.redirect_stdout(output):
    import yt_dlp
    from yt_dlp.globals import plugin_dirs
    from yt_dlp.plugins import load_all_plugins

    def extract(ydl, loudness, request):
        output.seek(0)
        output.truncate()
        ydl.params['format'] = request['format']
        ydl.format_selector = ydl.build_format_selector(request['format'])
        # A failed download must not poison later requests on this instance.
        ydl._download_retcode = 0
        try:
            status = ydl.download([request['url']])
            urls = output.getvalue().splitlines()
            if status != 0 or not urls:
                urls = None
        except Exception:
            # Extractor diagnostics can contain cookies and signed URLs.
            urls = None
        # Always called, so one request's responses never reach the next.
        levels = loudness()
        return {'urls': urls, 'loudness': levels} if urls else {'urls': None}

    parsed = yt_dlp.parse_options(sys.argv[1:])
    # Match the CLI initialization order.
    plugin_dirs.value = parsed.options.plugin_dirs
    load_all_plugins()
    with yt_dlp.YoutubeDL(parsed.ydl_opts) as ydl:
        remember_player_scripts(ydl)
        loudness = report_loudness(ydl)
        if parsed.ydl_opts['allowed_extractors'] == ['^ytmusicaccount$']:
            if (set(ydl._ies) != {'YtMusicAccount'}
                    or ydl.get_info_extractor('YtMusicAccount').IE_NAME != 'ytmusicaccount'):
                raise RuntimeError('Account extractor is unavailable')
        protocol.write('{"ready":true}\n')
        protocol.flush()
        for line in sys.stdin:
            try:
                request = json.loads(line)
                response = extract(ydl, loudness, request)
            except Exception:
                response = {'urls': None}
            protocol.write(json.dumps(response) + '\n')
            protocol.flush()
