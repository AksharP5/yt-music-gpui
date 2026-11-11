"""Private, sequential --get-url server. All output stays in the parent's pipe."""

import contextlib
import io
import json
import sys

# A standalone yt-dlp zipapp carries its own package. For script installs this
# entry is ignored and the shebang's interpreter supplies the installed package.
sys.path.insert(0, sys.argv.pop(1))

protocol = sys.stdout
output = io.StringIO()
with contextlib.redirect_stdout(output):
    import yt_dlp
    from yt_dlp.globals import plugin_dirs
    from yt_dlp.plugins import load_all_plugins

    parsed = yt_dlp.parse_options(sys.argv[1:])
    # Match the CLI initialization order. YoutubeDL's API defaults would also
    # search unrelated user plugins, and would not apply the CLI plugin dirs.
    plugin_dirs.value = parsed.options.plugin_dirs
    load_all_plugins()
    with yt_dlp.YoutubeDL(parsed.ydl_opts) as ydl:
        if parsed.ydl_opts['allowed_extractors'] == ['^ytmusicaccount$']:
            if (set(ydl._ies) != {'YtMusicAccount'}
                    or ydl.get_info_extractor('YtMusicAccount').IE_NAME != 'ytmusicaccount'):
                raise RuntimeError('Account extractor is unavailable')
        protocol.write('{"ready":true}\n')
        protocol.flush()
        for line in sys.stdin:
            output.seek(0)
            output.truncate()
            try:
                request = json.loads(line)
                ydl.params['format'] = request['format']
                ydl.format_selector = ydl.build_format_selector(request['format'])
                # A failed download must not poison later requests on this instance.
                ydl._download_retcode = 0
                status = ydl.download([request['url']])
                urls = output.getvalue().splitlines()
                response = {'urls': urls} if status == 0 and urls else {'urls': None}
            except Exception:
                # Extractor diagnostics can contain cookies and signed URLs.
                response = {'urls': None}
            protocol.write(json.dumps(response) + '\n')
            protocol.flush()
