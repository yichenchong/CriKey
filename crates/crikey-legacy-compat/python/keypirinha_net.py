"""Documented ``keypirinha_net`` module of the CriKey Legacy Compatibility Layer.

The documented network module unchanged legacy plugins import when they need to
talk HTTP (spec 14.2). It is a **request builder**, and that is the whole point
of its shape:

* Importing this module performs no I/O, and neither does building a request or
  an opener. Every function here is pure apart from reading
  :data:`DEFAULT_TIMEOUT` and the product version. A plugin that builds a
  request during ``on_catalog`` therefore cannot stall the catalog scan on a
  DNS lookup, and CriKey's own test suite can pin that by poisoning
  ``socket.socket`` before the import and watching nothing fire.
* A URL is validated *at build time*, where the plugin's own stack frame is
  still on the traceback, instead of failing several layers deep inside
  ``urllib`` at open time where the diagnostic names nobody.
* The transport itself stays ``urllib``: :func:`build_urllib_opener` returns a
  real :class:`urllib.request.OpenerDirector`, so unchanged plugins keep using
  the API they were written against and CriKey adds no dependency of its own.

Only ``http`` and ``https`` are accepted. ``file://`` in particular is refused:
a legacy plugin handed an attacker-controlled URL must not be able to read the
user's filesystem through what the plugin's author believed was a web request.
"""

import math
import socket
import ssl
import urllib.parse
import urllib.request

import keypirinha as _keypirinha

__all__ = (
    "DEFAULT_TIMEOUT",
    "InvalidUrlError",
    "Request",
    "user_agent",
    "build_request",
    "build_urllib_opener",
)

#: Default per-request timeout, in seconds.
#:
#: A float, not an int: it is handed straight to ``urllib``'s ``timeout``
#: parameter, and a plugin that compares it against a float literal should not
#: have to think about which it got. Never ``None`` — an untimed request in a
#: worker process is an indefinitely stuck callback, which the host can only
#: resolve by killing the worker.
DEFAULT_TIMEOUT = 10.0

#: The schemes a request may use.
_ALLOWED_SCHEMES = frozenset(("http", "https"))

#: Canonical spelling of the user-agent header, used for lookups that must not
#: depend on how the caller spelled it.
_USER_AGENT = "User-Agent"


class InvalidUrlError(_keypirinha.KeypirinhaError, ValueError):
    """A URL cannot be requested, reported where the plugin built it.

    ``ValueError`` because that is what a plugin validating its own input
    already catches, and part of the one CriKey error taxonomy (spec 26.2) so
    the layer has a single family to report on.
    """

    def __init__(self, url, reason):
        self.url = url
        self.reason = reason
        _keypirinha.KeypirinhaError.__init__(
            self, "{!r} is not a requestable URL: {}".format(url, reason)
        )


def user_agent():
    """The ``User-Agent`` CriKey presents.

    Names CriKey and its version, and deliberately never the Keypirinha
    product name (spec 14.13): CriKey is an independent implementation, and a
    server operator reading its logs must not be told otherwise, however
    convenient impersonating a known client would be for compatibility.
    """
    return "CriKey/{}".format(_keypirinha.version_string())


#: Module-level alias so :func:`build_request` can reach the default agent
#: even though its own `user_agent` parameter — the documented spelling —
#: shadows the function inside that scope.
_DEFAULT_USER_AGENT = user_agent


def _canonical_header_name(name):
    """Canonical HTTP casing for `name`.

    Header names are case-insensitive on the wire but *not* in a Python dict,
    so they are normalised once on the way in. Without this, a plugin that
    passed ``user-agent`` and then read ``headers["User-Agent"]`` would see a
    ``KeyError`` and conclude the header was never set.
    """
    return "-".join(part.capitalize() for part in name.split("-"))


def _validate_url(url):
    """Returns `url` unchanged, or raises :class:`InvalidUrlError`.

    Pure parsing: :func:`urllib.parse.urlsplit` resolves nothing and looks
    nothing up, so validation stays free of I/O.
    """
    if not isinstance(url, str) or not url:
        raise InvalidUrlError(url, "the URL is empty")

    try:
        parts = urllib.parse.urlsplit(url)
    except ValueError as error:
        raise InvalidUrlError(url, "it cannot be parsed: {}".format(error)) from None

    if not parts.scheme:
        raise InvalidUrlError(
            url, "it carries no scheme; an absolute http or https URL is required"
        )
    if parts.scheme.lower() not in _ALLOWED_SCHEMES:
        raise InvalidUrlError(
            url,
            "the {!r} scheme is not requestable; only {} are".format(
                parts.scheme, " and ".join(sorted(_ALLOWED_SCHEMES))
            ),
        )
    if not parts.netloc or not parts.hostname:
        raise InvalidUrlError(url, "it names no host")
    try:
        parts.port
    except ValueError as error:
        raise InvalidUrlError(url, "its port is invalid: {}".format(error)) from None
    return url

def _timeout_value(timeout):
    """Returns a finite, positive timeout suitable for urllib."""
    if timeout is None:
        return DEFAULT_TIMEOUT
    try:
        value = float(timeout)
    except (TypeError, ValueError):
        raise ValueError("timeout must be a finite positive number") from None
    if not math.isfinite(value) or value <= 0.0:
        raise ValueError("timeout must be a finite positive number")
    return value


class _SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Follows only safe redirects that stay inside supported URL schemes."""

    handler_order = 400

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is None:
            return None
        _validate_url(redirected.full_url)
        source_scheme = urllib.parse.urlsplit(req.full_url).scheme.lower()
        target_scheme = urllib.parse.urlsplit(redirected.full_url).scheme.lower()
        if source_scheme == "https" and target_scheme == "http":
            raise InvalidUrlError(
                redirected.full_url,
                "an HTTPS request cannot redirect down to plain HTTP",
            )
        return redirected


def _install_timeout(opener, timeout):
    """Makes an opener apply a finite timeout when callers omit one."""
    original_open = opener.open

    def open_with_timeout(fullurl, *args, **kwargs):
        positional = list(args)
        if len(positional) >= 2:
            if positional[1] is None or positional[1] is socket._GLOBAL_DEFAULT_TIMEOUT:
                positional[1] = timeout
        elif kwargs.get("timeout") in (None, socket._GLOBAL_DEFAULT_TIMEOUT):
            kwargs["timeout"] = timeout
        return original_open(fullurl, *positional, **kwargs)

    opener.open = open_with_timeout
    return opener



class Request:
    """One prepared HTTP request. Building it performs no I/O.

    A plain value, not a connection: :attr:`url`, :attr:`headers`,
    :attr:`timeout` and :attr:`user_agent` are exactly what was asked for, so
    a plugin can inspect or log a request without sending it, and CriKey's
    diagnostics can report what a plugin *intended* to fetch even when the
    fetch never happened.
    """

    __slots__ = ("url", "headers", "timeout", "user_agent")

    def __init__(self, url, headers, timeout, agent):
        self.url = url
        #: Header names in canonical HTTP casing. Its own dict, so a caller
        #: mutating the mapping it passed in cannot retroactively change a
        #: request that was already built.
        self.headers = dict(headers)
        self.timeout = timeout
        self.user_agent = agent

    def get_header(self, name, default=None):
        """The value of `name`, ignoring case, or `default`.

        Case-insensitive because HTTP is: a plugin reading back
        ``user-agent`` after setting ``User-Agent`` is asking about the same
        header, and answering ``None`` there would be a lie about the wire.
        """
        canonical = _canonical_header_name(name)
        if canonical in self.headers:
            return self.headers[canonical]
        folded = name.lower()
        for key, value in self.headers.items():
            if key.lower() == folded:
                return value
        return default

    def __repr__(self):
        return "Request(url={!r}, timeout={!r}, headers={!r})".format(
            self.url, self.timeout, sorted(self.headers)
        )


def build_request(url, headers=None, timeout=None, user_agent=None):
    """Validates `url` and returns a :class:`Request` for it.

    Performs no network I/O whatsoever — no lookup, no connection, no probe.

    User-agent precedence, most specific first: the explicit `user_agent`
    argument, then a ``user-agent`` entry in `headers` (however spelled), then
    :func:`user_agent`. Whichever wins is stored under the canonical
    ``User-Agent`` header *and* reported as :attr:`Request.user_agent`, so the
    two can never disagree about what will go on the wire.

    An omitted `timeout` becomes :data:`DEFAULT_TIMEOUT` rather than ``None``.
    """
    validated = _validate_url(url)

    prepared = {}
    from_headers = None
    for name, value in (headers or {}).items():
        canonical = _canonical_header_name(name)
        if canonical == _USER_AGENT:
            from_headers = value
            continue
        prepared[canonical] = value

    agent = user_agent if user_agent is not None else from_headers
    if agent is None:
        agent = _DEFAULT_USER_AGENT()
    prepared[_USER_AGENT] = agent

    request_timeout = _timeout_value(timeout)
    return Request(
        validated,
        prepared,
        request_timeout,
        agent,
    )


def build_urllib_opener(
    proxies=None,
    ssl_check_hostname=None,
    extra_handlers=(),
    extra_pre_handlers=(),
    *,
    agent=None,
    handlers=None,
):
    """Builds an opener with an applied user agent, proxy and timeout policy.

    ``proxies`` follows :class:`urllib.request.ProxyHandler`: ``None`` keeps
    the process environment's proxy policy, while a mapping (including an
    empty mapping) explicitly supplies the policy. ``extra_handlers`` and
    ``extra_pre_handlers`` use the original Keypirinha names. ``handlers`` is
    a keyword-only alias retained for the first M3 shim.
    """
    if handlers is not None:
        if extra_handlers:
            raise TypeError("pass either handlers or extra_handlers, not both")
        extra_handlers = handlers

    own_handlers = [_SafeRedirectHandler()]
    if proxies is not None:
        own_handlers.insert(0, urllib.request.ProxyHandler(proxies))

    if ssl_check_hostname is not None:
        if not isinstance(ssl_check_hostname, bool):
            raise TypeError("ssl_check_hostname must be a bool or None")
        context = ssl.create_default_context()
        context.check_hostname = ssl_check_hostname
        https_handler = urllib.request.HTTPSHandler(context=context)
        https_handler.handler_order = 400
        own_handlers.append(https_handler)

    opener = urllib.request.build_opener(
        *tuple(extra_pre_handlers), *own_handlers, *tuple(extra_handlers)
    )
    opener.addheaders = [(_USER_AGENT, user_agent() if agent is None else agent)]
    return _install_timeout(opener, DEFAULT_TIMEOUT)


# --------------------------------------------------------------------------
# The undocumented-internal guard (spec 14.12)
# --------------------------------------------------------------------------


def __getattr__(name):
    """Turns a reach for an undocumented internal into an attributable report."""
    if name.startswith("__") and name.endswith("__"):
        raise AttributeError(name)
    raise _keypirinha.UndocumentedApiError("keypirinha_net", name)
