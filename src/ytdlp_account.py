"""Account-bound playback for the yt-dlp version exercised by this application."""

import copy
import fcntl
import functools
import json
import os
import re
import stat
import urllib.error
import urllib.request
from urllib.parse import urlsplit

from yt_dlp.extractor.youtube import YoutubeIE
from yt_dlp.networking._urllib import UrllibRH
from yt_dlp.utils import DownloadError
from yt_dlp.utils.traversal import traverse_obj
from yt_dlp.version import __version__

# Some portable CPython builds omit these stable Linux fcntl ABI constants.
_F_GET_SEALS = getattr(fcntl, "F_GET_SEALS", 1034)
_REQUIRED_SEALS = 0x0f


class _NoAccountRedirect(urllib.request.HTTPRedirectHandler):
    handler_order = 100

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, "Account playback refused a redirect", headers, fp)


class _AccountUrllibRH(UrllibRH):
    def _create_instance(self, *args, **kwargs):
        opener = super()._create_instance(*args, **kwargs)
        opener.add_handler(_NoAccountRedirect())
        return opener


class YtMusicAccountIE(YoutubeIE):
    IE_NAME = "ytmusicaccount"
    _VALID_URL = r"https://music\.youtube\.com/watch\?v=(?P<id>[A-Za-z0-9_-]{11})$"
    _LOGIN_REQUIRED = True

    @functools.cached_property
    def _account(self):
        if __version__ != "2026.08.19":
            raise DownloadError("Account playback requires the reviewed yt-dlp version")
        paths = self._configuration_arg("context", ie_key="ytmusicaccount", casesense=True)
        try:
            if len(paths) != 1 or not re.fullmatch(r"/proc/[0-9]+/fd/[0-9]+", paths[0]):
                raise ValueError
            with open(paths[0], "rb") as file:
                metadata = os.fstat(file.fileno())
                if (stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_uid != os.geteuid()
                        or metadata.st_size > 4096 or metadata.st_nlink != 0
                        or fcntl.fcntl(file, _F_GET_SEALS) & _REQUIRED_SEALS != _REQUIRED_SEALS):
                    raise ValueError
                account = json.load(file)
            if set(account) != {"authuser", "delegated_session_id", "data_sync_id"}:
                raise ValueError
            index = account["authuser"]
            if not isinstance(index, str) or not re.fullmatch(r"[0-9]{1,10}", index) or int(index) > 0xffffffff:
                raise ValueError
            match = re.fullmatch(r"([A-Za-z0-9_-]{1,256})\|\|([A-Za-z0-9_-]{0,256})", account["data_sync_id"])
            if not match:
                raise ValueError
            first, second = match.groups()
            delegated = first if second else None
            if account["delegated_session_id"] != delegated:
                raise ValueError
            return {**account, "authuser": int(index), "user_session_id": second or first}
        except (OSError, ValueError, TypeError, KeyError):
            raise DownloadError("Playback account identity is unavailable or invalid") from None

    def _real_initialize(self):
        self._account
        # One reviewed transport, with redirects rejected before a second destination is contacted.
        self._downloader._request_director.close()
        self._downloader._request_director = self._downloader.build_request_director([_AccountUrllibRH])
        super()._real_initialize()

    def _check_identity(self, *values):
        expected = {
            "SESSION_INDEX": str(self._account["authuser"]),
            "DELEGATED_SESSION_ID": self._account["delegated_session_id"],
            "USER_SESSION_ID": self._account["user_session_id"],
            "DATASYNC_ID": self._account["data_sync_id"],
        }
        for value in values:
            if not isinstance(value, dict):
                continue
            logged_out = traverse_obj(value, ("responseContext", "mainAppWebResponseContext", "loggedOut"))
            if value.get("LOGGED_IN") is False or logged_out is True:
                raise DownloadError("YouTube rejected the playback account session")
            reported = traverse_obj(value, ("responseContext", "mainAppWebResponseContext", "datasyncId"))
            if reported is not None and reported != self._account["data_sync_id"]:
                raise DownloadError("YouTube returned a different playback account")
            for key, selected in expected.items():
                if key not in value:
                    continue
                actual = value[key]
                if key == "SESSION_INDEX":
                    actual = str(actual)
                if actual != selected:
                    raise DownloadError("YouTube returned a different playback account")

    def extract_ytcfg(self, *args, **kwargs):
        config = super().extract_ytcfg(*args, **kwargs)
        self._verify_server_identity(config)
        return config

    def _verify_server_identity(self, response):
        self._check_identity(response)
        if not isinstance(response, dict):
            return
        reported = (response.get("DATASYNC_ID"), traverse_obj(
            response, ("responseContext", "mainAppWebResponseContext", "datasyncId")))
        if self._account["data_sync_id"] in reported:
            self._server_identity_confirmed = True

    def _extract_session_index(self, *values):
        self._check_identity(*values)
        return self._account["authuser"]

    def _extract_delegated_session_id(self, *values):
        self._check_identity(*values)
        return self._account["delegated_session_id"]

    def _extract_user_session_id(self, *values):
        self._check_identity(*values)
        return self._account["user_session_id"]

    def _extract_data_sync_id(self, *values):
        self._check_identity(*values)
        return self._account["data_sync_id"]

    def _extract_response(self, *args, **kwargs):
        response = super()._extract_response(*args, **kwargs)
        self._verify_server_identity(response)
        return response

    def _extract_player_response(self, *args, **kwargs):
        response = super()._extract_player_response(*args, **kwargs)
        self._verify_server_identity(response)
        return response

    def _extract_player_responses(self, *args, **kwargs):
        responses, player_url = super()._extract_player_responses(*args, **kwargs)
        for response in responses:
            self._verify_server_identity(response)
        return responses, player_url

    def _generate_cookie_auth_headers(self, *, ytcfg=None, **kwargs):
        self._check_identity(ytcfg)
        # Discard request-local identity hints. Only the sealed context selects authority.
        kwargs.update(
            session_index=self._account["authuser"],
            delegated_session_id=self._account["delegated_session_id"],
            user_session_id=self._account["user_session_id"],
        )
        return super()._generate_cookie_auth_headers(ytcfg=ytcfg, **kwargs)

    def _extract_context(self, ytcfg=None, default_client="web"):
        self._check_identity(ytcfg)
        context = copy.deepcopy(super()._extract_context(ytcfg, default_client))
        delegated = self._account["delegated_session_id"]
        context["user"] = {"onBehalfOfUser": delegated} if delegated else {}
        return context

    def _request_webpage(self, url_or_request, *args, headers=None, **kwargs):
        request = self._create_request(url_or_request, headers=headers)
        target = urlsplit(request.url)
        if target.hostname == "youtube.com" or (target.hostname or "").endswith(".youtube.com"):
            if target.scheme != "https":
                raise DownloadError("Account playback requires HTTPS")
            if not self._has_auth_cookies:
                raise DownloadError("Playback account session expired. Connect again")
            request.headers.pop("X-Goog-PageId", None)
            request.headers.update(self._generate_cookie_auth_headers(origin=f"https://{target.netloc}"))
        return super()._request_webpage(request, *args, **kwargs)

    def _real_extract(self, url):
        self._server_identity_confirmed = False
        result = super()._real_extract(url)
        if not self._server_identity_confirmed:
            raise DownloadError("YouTube did not confirm the selected playback account")
        if result.get("_type", "video") != "video":
            raise DownloadError("Account playback cannot delegate to another extractor")
        return result
