"""Documented ``keypirinha_util`` module of the CriKey Legacy Compatibility Layer.

The documented helper module unchanged legacy plugins import for command-line
handling, environment expansion, directory scanning and the handful of
operations that reach the user's desktop session (spec 14.2).

The module falls into two halves, and the split is deliberate:

* **Pure string and filesystem helpers** — :func:`cmdline_split`,
  :func:`cmdline_quote`, :func:`expand_variables`, :func:`scan_directory` —
  behave *identically on every platform*. They implement the Win32 rules the
  plugins were written against, in Python, with no operating-system call
  behind them. A plugin's parsing must not change because CriKey happens to
  run on Linux today.
* **Desktop-touching helpers** — :func:`set_clipboard`, :func:`get_clipboard`,
  :func:`open_url`, :func:`shell_execute`, :func:`explore_file` — need a real
  desktop session. Where one is not reachable they raise
  :class:`UnavailableError` naming the operation and the platform, and they
  never pretend to have succeeded (spec 14.12, 26.2). A silent no-op here is
  the "plausible lie" the compatibility report exists to prevent: the plugin
  would look conformant while doing nothing at all.

:func:`desktop_available` exists so the diagnostics layer can classify a
plugin *without* provoking an exception, which is why it answers a bool rather
than raising.

Nothing in this module imports anything outside the standard library, and
nothing here performs network I/O.
"""

import enum
import codecs
import ctypes

import fnmatch
import os
import shlex
import shutil
import subprocess
import sys

import keypirinha as _keypirinha

#: Captured before :func:`decode_bytes` shadows the builtin with its
#: documented parameter name. `memoryview` and `bytearray` are accepted
#: because a plugin that read a file into one should not have to copy it back
#: just to name the type this layer expects.
_BYTES_TYPES = (bytes, bytearray, memoryview)
_AS_BYTES = bytes

__all__ = (
    "UnavailableError",
    "ScanFlags",
    "cmdline_split",
    "cmdline_quote",
    "expand_variables",
    "scan_directory",
    "desktop_available",
    "set_clipboard",
    "get_clipboard",
    "open_url",
    "shell_execute",
    "explore_file",
    "chardet_open",
    "decode_bytes",
    "kwargs_encode",
    "kwargs_decode",
    "execute_default_action",
    "web_browser_command",
    "read_link",
    "shell_known_folder_path",
)

#: Hard ceiling on how many entries one :func:`scan_directory` call retains.
#: A scan rooted at a filesystem root would otherwise grow without bound in a
#: plugin's callback, and an out-of-memory kill of the worker is a far worse
#: outcome than a truncated listing. Overflow is *reported*, never silent: see
#: :func:`scan_directory`.
_MAX_SCAN_RESULTS = 100_000

#: Longest text :func:`set_clipboard` will hand to a desktop helper. The
#: clipboard is a user-visible, fixed-purpose channel, not a data pipe, and an
#: unbounded write here would be an unbounded subprocess argument.
_MAX_CLIPBOARD_CHARS = 1 << 20

#: Maximum time a desktop helper may keep the worker blocked.
_HELPER_TIMEOUT_SECONDS = 10.0

#: Characters that force :func:`cmdline_quote` to quote an argument. A bare
#  backslash is *not* one of them: `C:\\dir` needs no quoting and quoting it
#  would break plugins that compare the result against a literal.
_MUST_QUOTE = (" ", "\t", '"')

_BACKSLASH = "\\"
_QUOTE = '"'


def _quote_one(argument, force_quote):
    """Quotes one validated command-line argument."""
    if force_quote or not argument or any(char in argument for char in _MUST_QUOTE):
        out = [_QUOTE]
        index = 0
        length = len(argument)
        while index < length:
            if argument[index] == _BACKSLASH:
                run = 0
                while index < length and argument[index] == _BACKSLASH:
                    run += 1
                    index += 1
                if index == length:
                    # Trailing backslashes sit immediately before the closing
                    # quote we are about to append, so they must be doubled or
                    # that quote would be read as escaped.
                    out.append(_BACKSLASH * (2 * run))
                elif argument[index] == _QUOTE:
                    out.append(_BACKSLASH * (2 * run + 1))
                    out.append(_QUOTE)
                    index += 1
                else:
                    out.append(_BACKSLASH * run)
                continue

            char = argument[index]
            out.append(_BACKSLASH + _QUOTE if char == _QUOTE else char)
            index += 1

        out.append(_QUOTE)
        return "".join(out)
    return argument

# --------------------------------------------------------------------------
# Honest unavailability (spec 14.12, 26.2)
# --------------------------------------------------------------------------


class UnavailableError(_keypirinha.KeypirinhaError, RuntimeError):
    """A helper cannot run here, and says so instead of pretending.

    ``RuntimeError`` by design, and deliberately **not** ``AttributeError`` or
    ``OSError``:

    * ``AttributeError`` would be swallowed by ``hasattr`` and
      ``getattr(..., default)``, the two probes plugins reach for first, and
      an honest "cannot" would become a silent ``False``.
    * ``OSError`` is routinely caught wholesale around file and process work,
      so an ``OSError`` subclass would be absorbed by the very ``except``
      blocks that surround these calls and the layer would have nothing to
      report.

    Carries :attr:`operation`, :attr:`platform` and :attr:`reason` separately
    from the message so the diagnostics layer can classify without parsing
    English.
    """

    def __init__(self, operation, reason, platform=None):
        self.operation = operation
        self.platform = sys.platform if platform is None else platform
        self.reason = reason
        _keypirinha.KeypirinhaError.__init__(
            self,
            "keypirinha_util.{}() is unavailable on platform {}: {}".format(
                operation, self.platform, reason
            ),
        )


# --------------------------------------------------------------------------
# Command lines (CommandLineToArgvW rules, on every platform)
# --------------------------------------------------------------------------


def cmdline_split(cmdline):
    """Splits `cmdline` the way ``CommandLineToArgvW`` does.

    Implemented in pure Python rather than delegated to the operating system
    because it is *parsing*, and unchanged plugins were written against these
    exact rules. Delegating would make a plugin's argument handling depend on
    which host it runs on, which is the one thing the compatibility layer
    exists to prevent.

    The rules, in the order they apply:

    * unquoted whitespace (space or tab) separates arguments, and runs of it
      collapse;
    * ``"`` toggles quoting and is not itself part of the argument;
    * ``2n`` backslashes before a ``"`` are ``n`` literal backslashes and the
      quote keeps its meaning; ``2n+1`` are ``n`` literal backslashes and a
      literal quote;
    * backslashes not followed by a quote are literal, however many there are.

    A quote alone is enough to *start* an argument, which is what makes
    ``"" x`` two arguments — the first empty — rather than one.
    """
    arguments = []
    current = []
    started = False
    in_quotes = False
    index = 0
    length = len(cmdline)

    while index < length:
        char = cmdline[index]

        if char == _BACKSLASH:
            run = 0
            while index < length and cmdline[index] == _BACKSLASH:
                run += 1
                index += 1
            if index < length and cmdline[index] == _QUOTE:
                current.append(_BACKSLASH * (run // 2))
                if run % 2:
                    # Odd run: the quote was escaped and is literal text.
                    current.append(_QUOTE)
                    index += 1
                # Even run: the quote survives as a delimiter and is handled
                # by the next iteration, so it is deliberately not consumed.
            else:
                current.append(_BACKSLASH * run)
            started = True
            continue

        if char == _QUOTE:
            in_quotes = not in_quotes
            started = True
            index += 1
            continue

        if not in_quotes and (char == " " or char == "\t"):
            if started:
                arguments.append("".join(current))
                current = []
                started = False
            index += 1
            continue

        current.append(char)
        started = True
        index += 1

    if started:
        arguments.append("".join(current))
    return arguments


def cmdline_quote(arg_or_list, force_quote=False):
    """Joins and quotes arguments using ``CommandLineToArgvW`` rules.

    ``arg_or_list`` accepts one string, or a list/tuple of strings. The
    optional ``force_quote`` flag requests quoting even when an argument has
    no whitespace or embedded quote. That flag is part of the documented
    API: callers use it when a command-line consumer requires every argument
    to be delimited.
    """
    if isinstance(arg_or_list, str):
        arguments = [arg_or_list]
    elif isinstance(arg_or_list, (list, tuple)):
        arguments = list(arg_or_list)
    else:
        raise TypeError("invalid args type")

    for argument in arguments:
        if not isinstance(argument, str):
            raise TypeError("arguments must be strings")
    return " ".join(_quote_one(argument, force_quote) for argument in arguments)


# --------------------------------------------------------------------------
# Environment expansion
# --------------------------------------------------------------------------


def _folded_lookup(*mappings):
    """Case-insensitive view over `mappings`, earlier ones winning.

    Windows environment lookup ignores case and unchanged plugins rely on it,
    so `%path%` and `%PATH%` must name the same variable on every host.
    """
    folded = {}
    for mapping in mappings:
        if not mapping:
            continue
        for key, value in mapping.items():
            folded.setdefault(key.lower(), value)
    return folded


def expand_variables(text, custom_vars=None, environ=None):
    """Expands Windows-style ``%VAR%`` references in `text`.

    * ``%%`` is a literal percent sign.
    * `custom_vars` is consulted before the environment, so a caller can
      override or add variables without touching the process environment.
    * Lookup ignores case.
    * An **unknown** variable is left exactly as written, percent signs
      included. Expanding it to an empty string — which is what the Windows
      shell does — silently turns ``%TYPO%\\sub`` into ``\\sub``, and a path
      that quietly lost its prefix is far harder to diagnose than one that
      still shows the marker that failed.
    * An unterminated ``%`` is likewise left verbatim.
    """
    if not text:
        return text

    variables = _folded_lookup(custom_vars, os.environ if environ is None else environ)
    out = []
    index = 0
    length = len(text)

    while index < length:
        char = text[index]
        if char != "%":
            out.append(char)
            index += 1
            continue

        if text[index + 1 : index + 2] == "%":
            out.append("%")
            index += 2
            continue

        closing = text.find("%", index + 1)
        if closing == -1:
            out.append(text[index:])
            break

        value = variables.get(text[index + 1 : closing].lower())
        out.append(text[index : closing + 1] if value is None else value)
        index = closing + 1

    return "".join(out)


# --------------------------------------------------------------------------
# Directory scanning
# --------------------------------------------------------------------------


class ScanFlags(enum.IntFlag):
    """The flags accepted by :func:`scan_directory`.

    ``DIRS`` is the spelling used by the original API; ``FOLDERS`` and
    ``RECURSIVE`` are retained as compatibility extensions used by CriKey
    packages that adopted the first M3 shim.
    """

    FILES = 0x01
    DIRS = 0x02
    FOLDERS = DIRS
    HIDDEN = 0x04
    CASE_SENSITIVE = 0x08
    ABORT_ON_ERROR = 0x10
    RECURSIVE = 0x20
    DEFAULT = FILES | DIRS


def scan_directory(base_dir, name_patterns="*", flags=ScanFlags.DEFAULT, max_level=0):
    """Walks `base_dir` and returns matching paths relative to it.

    Matching uses the entry name, not the complete relative path. `max_level`
    follows the documented convention: zero scans only the immediate contents,
    one also scans the contents of immediate subdirectories, and a negative
    value is unlimited. The M3-only ``RECURSIVE`` flag remains accepted and
    means unlimited depth when ``max_level`` is left at zero.

    Missing or unreadable roots raise ``OSError``. An unreadable descendant is
    skipped unless ``ABORT_ON_ERROR`` is set, matching the original helper's
    error contract. Results are capped to protect the worker from an
    accidentally unbounded filesystem walk.
    """
    patterns = (
        (name_patterns,) if isinstance(name_patterns, str) else tuple(name_patterns)
    )
    if not patterns:
        patterns = ("*",)

    want_files = bool(flags & ScanFlags.FILES)
    want_folders = bool(flags & ScanFlags.DIRS)
    case_sensitive = bool(flags & ScanFlags.CASE_SENSITIVE)
    abort_on_error = bool(flags & ScanFlags.ABORT_ON_ERROR)
    recursive_flag = bool(flags & ScanFlags.RECURSIVE)
    if recursive_flag and max_level == 0:
        max_level = -1

    def matches(name):
        if case_sensitive:
            return any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns)
        folded_name = name.casefold()
        return any(fnmatch.fnmatchcase(folded_name, pattern.casefold()) for pattern in patterns)

    results = []
    truncated = False
    pending = [("", 0)]

    while pending and not truncated:
        relative_dir, depth = pending.pop()
        absolute = os.path.join(base_dir, relative_dir) if relative_dir else base_dir
        try:
            with os.scandir(absolute) as entries:
                children = []
                for entry in entries:
                    try:
                        is_dir = entry.is_dir(follow_symlinks=False)
                    except OSError:
                        if abort_on_error:
                            raise
                        continue
                    children.append((entry.name, is_dir))
        except OSError:
            if not relative_dir or abort_on_error:
                raise
            continue

        for name, is_dir in sorted(children):
            if not (flags & ScanFlags.HIDDEN) and name.startswith("."):
                continue
            relative = os.path.join(relative_dir, name) if relative_dir else name

            if (is_dir and want_folders) or (not is_dir and want_files):
                if matches(name):
                    if len(results) >= _MAX_SCAN_RESULTS:
                        truncated = True
                        break
                    results.append(relative)

            if is_dir and (recursive_flag or max_level != 0) and (
                max_level < 0 or depth < max_level
            ):
                pending.append((relative, depth + 1))

    if truncated:
        sys.stderr.write(
            "[warn][keypirinha_util] scan_directory({!r}) stopped at the {} entry cap; "
            "the returned list is truncated\n".format(base_dir, _MAX_SCAN_RESULTS)
        )
        sys.stderr.flush()

    return sorted(results)


# --------------------------------------------------------------------------
# The desktop session
# --------------------------------------------------------------------------


def desktop_available():
    """Whether the desktop-touching helpers below can do anything at all.

    Answers a bool instead of raising so the diagnostics layer can classify a
    plugin's dependency without provoking an exception, and so a plugin can
    branch on it the way ``keypirinha_wintypes.is_available()`` is branched on.

    On POSIX a desktop session is an X11 or Wayland display; with neither
    there is no clipboard, no browser to hand a URL to and no file manager.
    """
    if sys.platform.startswith("win"):
        return True
    if sys.platform == "darwin":
        return True
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))


def _require_desktop(operation):
    """Refuses `operation` when no desktop session is reachable."""
    if not desktop_available():
        raise UnavailableError(
            operation,
            "no desktop session is reachable: neither DISPLAY nor WAYLAND_DISPLAY "
            "is set in this process environment",
        )


def _require_helper(operation, *candidates):
    """The first of `candidates` present on ``PATH``, or a typed refusal.

    POSIX has no clipboard or shell-execute system call: every one of these
    operations is an external helper, and a missing helper is exactly as
    unavailable as a missing display.
    """
    for candidate in candidates:
        found = shutil.which(candidate)
        if found:
            return found
    raise UnavailableError(
        operation,
        "none of the required desktop helpers ({}) is installed".format(
            ", ".join(candidates)
        ),
    )


def _run_helper(operation, argv, stdin_text=None, capture=False):
    """Runs a desktop helper, turning its failure into a typed refusal.

    Explicit return-code checking rather than ``check=True``: a
    ``CalledProcessError`` escaping from here is an exception a plugin's
    ``except OSError`` misses and its bare ``except`` misattributes. A finite
    timeout keeps a wedged clipboard or browser helper from pinning the worker.
    """
    try:
        completed = subprocess.run(
            argv,
            input=stdin_text,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=_HELPER_TIMEOUT_SECONDS,
            check=False,
        )
    except subprocess.TimeoutExpired:
        raise UnavailableError(
            operation,
            "the desktop helper {!r} exceeded the {} second timeout".format(
                argv[0], _HELPER_TIMEOUT_SECONDS
            ),
        ) from None
    except OSError as error:
        raise UnavailableError(
            operation,
            "the desktop helper {!r} could not be run: {}".format(argv[0], error),
        ) from None
    if completed.returncode != 0:
        raise UnavailableError(
            operation,
            "the desktop helper {!r} exited {}".format(argv[0], completed.returncode),
        )
    return completed.stdout if capture else None


def set_clipboard(text):
    """Replaces the desktop clipboard's text content."""
    if not isinstance(text, str):
        raise TypeError("clipboard text must be a string")
    _require_desktop("set_clipboard")
    if len(text) > _MAX_CLIPBOARD_CHARS:
        raise UnavailableError(
            "set_clipboard",
            "the text is {} characters, above the {} character clipboard bound".format(
                len(text), _MAX_CLIPBOARD_CHARS
            ),
        )
    if sys.platform.startswith("win"):
        _win32_set_clipboard(text)
        return
    if sys.platform == "darwin":
        _run_helper("set_clipboard", [_require_helper("set_clipboard", "pbcopy")], text)
        return
    helper = _require_helper("set_clipboard", "wl-copy", "xclip", "xsel")
    base = os.path.basename(helper)
    if base == "wl-copy":
        argv = [helper]
    elif base == "xclip":
        argv = [helper, "-selection", "clipboard"]
    else:
        argv = [helper, "--clipboard", "--input"]
    _run_helper("set_clipboard", argv, text)


def get_clipboard():
    """The desktop clipboard's text content."""
    _require_desktop("get_clipboard")
    if sys.platform.startswith("win"):
        return _win32_get_clipboard()
    if sys.platform == "darwin":
        return _run_helper(
            "get_clipboard", [_require_helper("get_clipboard", "pbpaste")], capture=True
        )
    helper = _require_helper("get_clipboard", "wl-paste", "xclip", "xsel")
    base = os.path.basename(helper)
    if base == "wl-paste":
        argv = [helper, "--no-newline"]
    elif base == "xclip":
        argv = [helper, "-selection", "clipboard", "-out"]
    else:
        argv = [helper, "--clipboard", "--output"]
    return _run_helper("get_clipboard", argv, capture=True)


def open_url(url):
    """Hands `url` to the desktop's default browser.

    The URL is *handed over*, never fetched: this module performs no network
    I/O of its own under any circumstances.
    """
    _require_desktop("open_url")
    if sys.platform.startswith("win"):
        _win32_shell_execute("open_url", url, "", "", "open")
        return
    opener = (
        _require_helper("open_url", "open")
        if sys.platform == "darwin"
        else _require_helper("open_url", "xdg-open", "gio")
    )
    argv = [opener, "open", url] if os.path.basename(opener) == "gio" else [opener, url]
    _run_helper("open_url", argv)


def shell_execute(target, args="", working_dir="", verb=""):
    """Launches `target` through the desktop shell.

    This is the documented ``ShellExecuteW`` wrapper, not a process spawner:
    it resolves file associations, honours verbs and may raise UI, all of
    which are desktop-session operations. That is why it is unavailable
    without a session even for a target that would run perfectly well
    headless — reporting success for a call that could not do what it
    documents is the failure mode this whole module is arranged against.
    """
    _require_desktop("shell_execute")
    if sys.platform.startswith("win"):
        _win32_shell_execute("shell_execute", target, args, working_dir, verb or "open")
        return
    opener = (
        _require_helper("shell_execute", "open")
        if sys.platform == "darwin"
        else _require_helper("shell_execute", "xdg-open", "gio")
    )
    argv = (
        [opener, "open", target]
        if os.path.basename(opener) == "gio"
        else [opener, target]
    )
    _run_helper("shell_execute", argv)


def explore_file(path):
    """Reveals `path` in the desktop's file manager."""
    _require_desktop("explore_file")
    if sys.platform.startswith("win"):
        _win32_explore_file(path)
        return
    if sys.platform == "darwin":
        _run_helper("explore_file", [_require_helper("explore_file", "open"), "-R", path])
        return
    helper = _require_helper("explore_file", "xdg-open", "gio")
    # A file manager is opened on the containing directory: `xdg-open` on a
    # file launches its associated application instead of revealing it, which
    # is a different operation with a different side effect.
    target = path if os.path.isdir(path) else os.path.dirname(os.path.abspath(path))
    argv = (
        [helper, "open", target]
        if os.path.basename(helper) == "gio"
        else [helper, target]
    )
    _run_helper("explore_file", argv)


# --------------------------------------------------------------------------
# Text decoding
#
# No charset detector is bundled and none is added: every usable one is a
# third-party package, and a compatibility layer that quietly required one
# would make the "CPython plus the standard library" interpreter requirement
# untrue for every host that installs CriKey. What is here is a ladder of
# *evidence*, and each rung says how much it actually knows.
# --------------------------------------------------------------------------

#: Byte-order marks, longest first. UTF-32-LE's mark begins with UTF-16-LE's,
#: so a shortest-first scan would read UTF-32-LE text as UTF-16-LE and produce
#: a string of interleaved NULs that decodes without raising.
#:
#: The codecs are the BOM-consuming spellings (`utf-16`, not `utf-16-le`), so
#: the mark does not survive into the text as a leading U+FEFF.
_BOMS = (
    (codecs.BOM_UTF32_LE, "utf-32"),
    (codecs.BOM_UTF32_BE, "utf-32"),
    (codecs.BOM_UTF8, "utf-8-sig"),
    (codecs.BOM_UTF16_LE, "utf-16"),
    (codecs.BOM_UTF16_BE, "utf-16"),
)

#: What unmarked, non-UTF-8 bytes are read as. Windows-1252 is the documented
#: guess and it is a guess: the legacy corpus is Windows software, and its
#: hand-edited configuration and data files are cp1252 when they are not
#: UTF-8. Nothing here can distinguish cp1252 from the other single-byte
#: encodings, and this docstring is the only honest place to say so.
_FALLBACK_ENCODING = "cp1252"

#: The last rung. Latin-1 maps all 256 byte values to code points, so it
#: cannot fail. It is reached only for the five byte values cp1252 leaves
#: undefined, and it preserves the bytes rather than claiming to know the text.
_LAST_RESORT_ENCODING = "latin-1"

#: How much of a file :func:`chardet_open` examines before deciding. Detection
#: must not read a whole file whose first line is all the caller wanted.
_MAX_DETECTION_BYTES = 1 << 20


def _detect_encoding(raw, truncated=False):
    """The encoding `raw` is in, by the documented ladder.

    `truncated` says `raw` is a bounded prefix of a longer input. A prefix can
    end in the middle of a multi-byte sequence, and treating that as evidence
    against UTF-8 would misread a large UTF-8 file as cp1252 purely because of
    where the probe stopped.
    """
    for mark, encoding in _BOMS:
        if raw.startswith(mark):
            return encoding

    try:
        raw.decode("utf-8")
        return "utf-8"
    except UnicodeDecodeError as error:
        # The longest UTF-8 sequence is four bytes, so only a failure inside
        # the final three can be an artefact of the cut.
        if truncated and error.start >= len(raw) - 3:
            return "utf-8"

    try:
        raw.decode(_FALLBACK_ENCODING)
        return _FALLBACK_ENCODING
    except UnicodeDecodeError:
        return _LAST_RESORT_ENCODING


def decode_bytes(bytes):
    """Decodes `bytes` to text, choosing the encoding as documented below.

    The ladder, in order:

    1. a byte-order mark, which is proof rather than a guess;
    2. UTF-8, accepted only when the *whole* input decodes, which for any
       non-trivial input is near-proof: other encodings produce invalid
       sequences almost immediately;
    3. Windows-1252, the documented guess for the legacy corpus;
    4. Latin-1, which cannot fail, for the byte values cp1252 leaves
       undefined.

    Rungs 3 and 4 are guesses and are labelled as such. This is not
    statistical charset detection and does not pretend to be — that needs a
    third-party detector this layer deliberately does not depend on.

    The parameter shadows the builtin because ``bytes`` is the documented
    parameter name and callers pass it by keyword.
    """
    if not isinstance(bytes, _BYTES_TYPES):
        raise TypeError(
            "decode_bytes() takes bytes, not {}".format(type(bytes).__name__)
        )
    raw = _AS_BYTES(bytes)
    return raw.decode(_detect_encoding(raw))


def chardet_open(file, mode="r", buffering=-1, encoding=None, errors=None, newline=None, **kwargs):
    """Opens `file` using the documented evidence-based encoding ladder.

    An explicit `encoding` is honoured; otherwise at most one megabyte is
    probed using :func:`decode_bytes`'s BOM/UTF-8/cp1252/Latin-1 ladder.
    """
    if not isinstance(mode, str):
        raise TypeError("chardet_open() mode must be a string")
    if "b" in mode:
        raise ValueError("chardet_open() opens text, not binary mode {!r}".format(mode))
    if encoding is None:
        with open(file, "rb") as probe:
            raw = probe.read(_MAX_DETECTION_BYTES)
        selected = _detect_encoding(raw, len(raw) == _MAX_DETECTION_BYTES)
    else:
        selected = encoding
    return open(
        file,
        mode,
        buffering=buffering,
        encoding=selected,
        errors=errors,
        newline=newline,
        **kwargs
    )


# --------------------------------------------------------------------------
# Packing keyword arguments into one string
# --------------------------------------------------------------------------

#: Separates one pair from the next.
_KWARGS_SEPARATOR = "&"

#: Separates a name from its value.
_KWARGS_ASSIGNMENT = "="

#: Escapes the separators and itself.
_KWARGS_ESCAPE = "\\"

_KWARGS_SPECIAL = frozenset((_KWARGS_ESCAPE, _KWARGS_SEPARATOR, _KWARGS_ASSIGNMENT))


def _kwargs_escape(text):
    return "".join(
        _KWARGS_ESCAPE + char if char in _KWARGS_SPECIAL else char for char in text
    )


def _kwargs_value_encode(value, name):
    if isinstance(value, bool):
        return "b:" + ("1" if value else "0")
    if isinstance(value, int):
        return "i:" + str(value)
    if isinstance(value, float):
        return "f:" + repr(value)
    if isinstance(value, str):
        return "s:" + value
    raise TypeError(
        "kwargs_encode() value for {!r} is {}; expected bool, int, float or str".format(
            name, type(value).__name__
        )
    )


def _kwargs_value_decode(value):
    if len(value) < 2 or value[1] != ":":
        raise ValueError("kwargs value has no type tag")
    tag, payload = value[0], value[2:]
    try:
        if tag == "s":
            return payload
        if tag == "b" and payload in ("0", "1"):
            return payload == "1"
        if tag == "i":
            return int(payload, 10)
        if tag == "f":
            return float(payload)
    except (TypeError, ValueError):
        pass
    raise ValueError("kwargs value has an invalid type tag")


def kwargs_encode(**kwargs):
    """Packs basic bool/int/float/str keyword arguments reversibly.

    Pair separators and escapes are escaped, while a one-character type tag
    keeps scalar values typed when :func:`kwargs_decode` reverses the string.
    """
    return _KWARGS_SEPARATOR.join(
        _kwargs_escape(name)
        + _KWARGS_ASSIGNMENT
        + _kwargs_escape(_kwargs_value_encode(kwargs[name], name))
        for name in sorted(kwargs)
    )


def kwargs_decode(text):
    """The exact inverse of :func:`kwargs_encode`, rejecting malformed text."""
    if not isinstance(text, str):
        raise TypeError("kwargs_decode() takes a string, not {}".format(type(text).__name__))
    if not text:
        return {}
    pairs = []
    name = None
    buffer = []
    index = 0
    while index < len(text):
        char = text[index]
        index += 1
        if char == _KWARGS_ESCAPE:
            if index >= len(text):
                raise ValueError("kwargs text ends with a lone escape character")
            buffer.append(text[index])
            index += 1
        elif char == _KWARGS_ASSIGNMENT:
            if name is not None:
                raise ValueError("kwargs pair carries a second unescaped '='")
            name = "".join(buffer)
            buffer = []
        elif char == _KWARGS_SEPARATOR:
            pairs.append((name, "".join(buffer)))
            name = None
            buffer = []
        else:
            buffer.append(char)
    pairs.append((name, "".join(buffer)))
    decoded = {}
    for name, value in pairs:
        if not name:
            raise ValueError("a kwargs pair carries no name")
        if name in decoded:
            raise ValueError("kwargs name {!r} appears twice".format(name))
        decoded[name] = _kwargs_value_decode(value)
    return decoded


# --------------------------------------------------------------------------
# Host-mediated execution
# --------------------------------------------------------------------------


def execute_default_action(plugin, item, action=None):
    """Asks the host to do with `item` what it would do had the user run it.

    The launcher performs the work, not this process, and that is the whole
    point of routing it through the host object. Two things depend on it: the
    launcher owns the permission gate on host-mediated process launches, and
    it owns a process group that outlives this worker — a browser opened by
    the worker would be killed with it the next time the plugin is reaped or
    restarted.

    Returns ``True`` when the host performed the action and ``False`` when it
    declined because `item` carries nothing it knows how to act on. A host
    that cannot perform host-mediated actions at all, or that refuses this
    plugin, raises :class:`keypirinha.HostUnavailableError`; it is never a
    silent no-op.
    """
    capability = _keypirinha._host_capability("execute_default_action")
    return bool(capability(plugin, item, action))


# --------------------------------------------------------------------------
# The web browser
# --------------------------------------------------------------------------

#: Private-mode and new-window flags, per browser, as ``(private, window)``.
#:
#: A table rather than a heuristic. The flags are browser-specific and getting
#: one wrong opens an ordinary window for a caller who asked for a private
#: one, which is a privacy failure dressed up as a convenience. An unlisted
#: browser is refused when either flag is requested, for the same reason.
_BROWSER_FLAGS = {
    "firefox": ("-private-window", "-new-window"),
    "firefox-esr": ("-private-window", "-new-window"),
    "librewolf": ("-private-window", "-new-window"),
    "waterfox": ("-private-window", "-new-window"),
    "chromium": ("--incognito", "--new-window"),
    "chromium-browser": ("--incognito", "--new-window"),
    "chrome": ("--incognito", "--new-window"),
    "google-chrome": ("--incognito", "--new-window"),
    "google-chrome-stable": ("--incognito", "--new-window"),
    "brave-browser": ("--incognito", "--new-window"),
    "vivaldi": ("--incognito", "--new-window"),
    "vivaldi-stable": ("--incognito", "--new-window"),
    "opera": ("--private", "--new-window"),
    "microsoft-edge": ("--inprivate", "--new-window"),
    "msedge": ("--inprivate", "--new-window"),
    "epiphany": ("--incognito-mode", "--new-window"),
}

#: Browsers looked for on ``PATH``, in order, when ``BROWSER`` names none that
#: exists. Ordered by how likely a desktop is to have made it the default.
_BROWSER_CANDIDATES = (
    "firefox",
    "chromium",
    "chromium-browser",
    "google-chrome",
    "chrome",
    "brave-browser",
    "vivaldi",
    "microsoft-edge",
    "msedge",
    "epiphany",
)


def _split_command(entry):
    """Splits one ``BROWSER`` entry into words, per platform quoting rules."""
    if sys.platform.startswith("win"):
        return cmdline_split(entry)
    return shlex.split(entry)


def _browser_entries():
    """Candidate browser commands, most authoritative first."""
    for entry in os.environ.get("BROWSER", "").split(os.pathsep):
        entry = entry.strip()
        if entry:
            yield entry
    for candidate in _BROWSER_CANDIDATES:
        yield candidate


def _browser_key(executable):
    """The `_BROWSER_FLAGS` key for a resolved browser path."""
    name = os.path.basename(executable).lower()
    root, extension = os.path.splitext(name)
    return root if extension == ".exe" else name


def web_browser_command(private_mode=False, new_window=False, url=None, execute=False):
    """The command line that opens `url` in this user's web browser.

    Returns an argv **list**, not one string. Documented Keypirinha returns a
    Windows command line; a list is unambiguous on every platform this layer
    runs on, and :func:`cmdline_quote` is right here for the callers that
    genuinely need the Windows spelling. This is the one deliberate
    difference and it is recorded in the compatibility matrix.

    The browser comes from ``BROWSER`` — the POSIX convention: a
    ``os.pathsep``-separated list of commands, an entry's ``%s`` replaced by
    the URL — and then from a list of known browsers on ``PATH``.

def web_browser_command(private_mode=None, new_window=None, url=None, execute=False):
    this layer knows, and *refused* with :class:`UnavailableError` for any
    other. Launching an unknown browser without the private-mode flag it was
    asked for would open an ordinary window the caller believes is private.

    `execute` runs the command as well as returning it, and then needs a
    desktop session like every other helper here.
    """
    executable, template = _resolve_browser()

    flags = []
    if private_mode or new_window:
        known = _BROWSER_FLAGS.get(_browser_key(executable))
        if known is None:
            raise UnavailableError(
                "web_browser_command",
                "the resolved browser {!r} is not one whose private-mode and "
                "new-window flags this layer knows, so the request cannot be "
                "honoured and is refused rather than dropped".format(executable),
            )
        private_flag, new_window_flag = known
        if private_mode:
            flags.append(private_flag)
        if new_window:
            flags.append(new_window_flag)

    argv = [executable]
    placed = False
    for token in template:
        if url is not None and "%s" in token:
            argv.append(token.replace("%s", url))
            placed = True
        else:
            argv.append(token)
    # Flags belong immediately after the executable: a browser reads them as
    # its own options only before the positional URL.
    argv[1:1] = flags
    if url is not None and not placed:
        argv.append(url)

    if execute:
        _require_desktop("web_browser_command")
        _run_helper("web_browser_command", argv)
    return argv


def _resolve_browser():
    """``(executable, template_arguments)`` for this user's browser."""
    for entry in _browser_entries():
        words = _split_command(entry)
        if not words:
            continue
        found = shutil.which(words[0])
        if found:
            return found, words[1:]
    raise UnavailableError(
        "web_browser_command",
        "no web browser was found: BROWSER names none that exists and none of "
        "{} is on PATH".format(", ".join(_BROWSER_CANDIDATES)),
    )


# --------------------------------------------------------------------------
# Windows shell services
#
# Both of these answer questions only the Windows shell can answer. They are
# refused off Windows rather than approximated: spec 2.3 forbids emulating a
# Windows API, and an approximation here would answer a *different* question
# under the same name, which a plugin would then act on (spec 14.12).
# --------------------------------------------------------------------------


def read_link(path):
    """Compatibility alias for :func:`keypirinha_wintypes.read_link`.

    Legacy packages import this helper from ``keypirinha_util``. The actual
    Windows-dependent operation is owned by the explicit platform interface;
    importing it lazily keeps the historical import path and avoids a module
    cycle during worker startup.
    """
    from keypirinha_wintypes import read_link as resolve

    return resolve(path)


def shell_known_folder_path(guid):
    """Compatibility alias for :func:`keypirinha_wintypes.shell_known_folder_path`."""
    from keypirinha_wintypes import shell_known_folder_path as resolve

    return resolve(guid)


# --------------------------------------------------------------------------
# Win32 implementations
#
# `ctypes` is imported lazily, inside these functions only: `ctypes.WinDLL`
# does not exist off Windows, and paying for the import on a host that can
# never reach these paths is pointless.
# --------------------------------------------------------------------------

#: `CF_UNICODETEXT`, the only clipboard format the documented API exposes.
_CF_UNICODETEXT = 13

#: `GMEM_MOVEABLE`. The clipboard takes ownership of the handle, so the memory
#: must be moveable global memory rather than a private allocation.
_GMEM_MOVEABLE = 0x0002

#: `SW_SHOWNORMAL`.
_SW_SHOWNORMAL = 1

#: Every `ShellExecuteW` return value at or below this is an error code.
_SE_ERROR_CEILING = 32


def _win32():
    return ctypes


def _win32_set_clipboard(text):
    ctypes = _win32()
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    user32.OpenClipboard.argtypes = (ctypes.c_void_p,)
    user32.OpenClipboard.restype = ctypes.c_int
    user32.EmptyClipboard.argtypes = ()
    user32.EmptyClipboard.restype = ctypes.c_int
    user32.SetClipboardData.argtypes = (ctypes.c_uint, ctypes.c_void_p)
    user32.SetClipboardData.restype = ctypes.c_void_p
    user32.CloseClipboard.argtypes = ()
    user32.CloseClipboard.restype = ctypes.c_int
    kernel32.GlobalAlloc.argtypes = (ctypes.c_uint, ctypes.c_size_t)
    kernel32.GlobalAlloc.restype = ctypes.c_void_p
    kernel32.GlobalLock.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalLock.restype = ctypes.c_void_p
    kernel32.GlobalUnlock.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalUnlock.restype = ctypes.c_int
    kernel32.GlobalFree.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalFree.restype = ctypes.c_void_p

    if not user32.OpenClipboard(None):
        raise UnavailableError(
            "set_clipboard", "the clipboard is locked by another process"
        )
    handle = None
    try:
        if not user32.EmptyClipboard():
            raise UnavailableError("set_clipboard", "EmptyClipboard refused the request")
        size = (len(text) + 1) * ctypes.sizeof(ctypes.c_wchar)
        handle = kernel32.GlobalAlloc(_GMEM_MOVEABLE, size)
        if not handle:
            raise UnavailableError("set_clipboard", "the clipboard allocation failed")
        buffer = kernel32.GlobalLock(handle)
        if not buffer:
            kernel32.GlobalFree(handle)
            handle = None
            raise UnavailableError("set_clipboard", "the clipboard allocation could not be locked")
        try:
            ctypes.memmove(buffer, ctypes.create_unicode_buffer(text), size)
        finally:
            kernel32.GlobalUnlock(handle)
        if not user32.SetClipboardData(_CF_UNICODETEXT, handle):
            raise UnavailableError("set_clipboard", "SetClipboardData refused the text")
        # Ownership transfers to the clipboard after a successful call.
        handle = None
    finally:
        if handle:
            kernel32.GlobalFree(handle)
        user32.CloseClipboard()


def _win32_get_clipboard():
    ctypes = _win32()
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    user32.IsClipboardFormatAvailable.argtypes = (ctypes.c_uint,)
    user32.IsClipboardFormatAvailable.restype = ctypes.c_int
    user32.OpenClipboard.argtypes = (ctypes.c_void_p,)
    user32.OpenClipboard.restype = ctypes.c_int
    user32.GetClipboardData.argtypes = (ctypes.c_uint,)
    user32.GetClipboardData.restype = ctypes.c_void_p
    user32.CloseClipboard.argtypes = ()
    user32.CloseClipboard.restype = ctypes.c_int
    kernel32.GlobalLock.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalLock.restype = ctypes.c_void_p
    kernel32.GlobalUnlock.argtypes = (ctypes.c_void_p,)
    kernel32.GlobalUnlock.restype = ctypes.c_int

    if not user32.IsClipboardFormatAvailable(_CF_UNICODETEXT):
        # Not a failure: a clipboard holding an image simply has no text.
        return ""
    if not user32.OpenClipboard(None):
        raise UnavailableError(
            "get_clipboard", "the clipboard is locked by another process"
        )
    try:
        handle = user32.GetClipboardData(_CF_UNICODETEXT)
        if not handle:
            return ""
        buffer = kernel32.GlobalLock(handle)
        if not buffer:
            raise UnavailableError("get_clipboard", "the clipboard data could not be locked")
        try:
            return ctypes.c_wchar_p(buffer).value or ""
        finally:
            kernel32.GlobalUnlock(handle)
    finally:
        user32.CloseClipboard()


def _win32_shell_execute(operation, target, args, working_dir, verb):
    ctypes = _win32()
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    shell32.ShellExecuteW.argtypes = (
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_int,
    )
    shell32.ShellExecuteW.restype = ctypes.c_void_p
    result = shell32.ShellExecuteW(
        None,
        ctypes.c_wchar_p(verb or None),
        ctypes.c_wchar_p(target),
        ctypes.c_wchar_p(args or None),
        ctypes.c_wchar_p(working_dir or None),
        _SW_SHOWNORMAL,
    )
    if not result or result <= _SE_ERROR_CEILING:
        raise UnavailableError(
            operation, "ShellExecuteW failed with code {}".format(result)
        )


def _win32_explore_file(path):
    ctypes = _win32()
    if os.path.isdir(path):
        _win32_shell_execute("explore_file", path, "", "", "explore")
        return
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    shell32.ShellExecuteW.argtypes = (
        ctypes.c_void_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_wchar_p,
        ctypes.c_int,
    )
    shell32.ShellExecuteW.restype = ctypes.c_void_p
    # `/select,<path>` must reach explorer.exe as one unquoted argument:
    # Explorer parses this argument itself and rejects a quoted form.
    result = shell32.ShellExecuteW(
        None,
        ctypes.c_wchar_p("open"),
        ctypes.c_wchar_p("explorer.exe"),
        ctypes.c_wchar_p("/select,{}".format(os.path.abspath(path))),
        None,
        _SW_SHOWNORMAL,
    )
    if not result or result <= _SE_ERROR_CEILING:
        raise UnavailableError(
            "explore_file", "ShellExecuteW failed with code {}".format(result)
        )


#: `CLSID_ShellLink`, `IID_IShellLinkW` and `IID_IPersistFile`, in the
#: registry string form `CLSIDFromString` parses. Written as strings rather
#: than hand-packed structures because a mistyped byte in a packed GUID fails
#: as "class not registered" at run time, which is a miserable thing to debug
#: on a platform this host cannot test on.
_CLSID_SHELL_LINK = "{00021401-0000-0000-C000-000000000046}"
_IID_ISHELLLINKW = "{000214F9-0000-0000-C000-000000000046}"
_IID_IPERSISTFILE = "{0000010B-0000-0000-C000-000000000046}"

#: `CLSCTX_INPROC_SERVER`; the shell link object is an in-process class.
_CLSCTX_INPROC_SERVER = 0x1

#: `COINIT_APARTMENTTHREADED`. The shell link object is an apartment-model
#: object, so the thread that creates it must be in an STA.
_COINIT_APARTMENTTHREADED = 0x2

#: `RPC_E_CHANGED_MODE`: this thread is already initialised, in the other
#: model. Not an error to us — we simply must not uninitialise a thread whose
#: apartment we did not enter.
_RPC_E_CHANGED_MODE = -2147417850

#: `STGM_READ`, and `SLGP_RAWPATH`: the stored path without the shell's
#: environment-variable expansion, which is what a caller asking to *read* a
#: link means by its target.
_STGM_READ = 0x0
_SLGP_RAWPATH = 0x4

#: `MAX_PATH`. `IShellLinkW::GetPath` will not write more than this however
#: large the buffer is.
_MAX_PATH = 260

#: Vtable slots. IUnknown occupies 0..2 in every interface; the rest are
#: counted from the interface's own declaration order.
_SLOT_QUERY_INTERFACE = 0
_SLOT_RELEASE = 2
_SLOT_PERSIST_FILE_LOAD = 5
_SLOT_SHELL_LINK_GET_PATH = 3


class _WinGuid(ctypes.Structure):
    """The Win32 `GUID` layout, for the by-reference COM arguments below."""

    _fields_ = (
        ("Data1", ctypes.c_uint32),
        ("Data2", ctypes.c_uint16),
        ("Data3", ctypes.c_uint16),
        ("Data4", ctypes.c_ubyte * 8),
    )


def _win32_guid(ole32, operation, text):
    """Parses a registry-form GUID string, or refuses it."""
    guid = _WinGuid()
    if ole32.CLSIDFromString(ctypes.c_wchar_p(text), ctypes.byref(guid)) != 0:
        raise UnavailableError(
            operation, "{!r} is not a GUID in the form {{...}}".format(text)
        )
    return guid


def _win32_com_method(interface, slot, *argtypes):
    """One COM method of `interface`, bound through its vtable."""
    vtable = ctypes.cast(
        interface, ctypes.POINTER(ctypes.POINTER(ctypes.c_void_p))
    ).contents
    prototype = ctypes.WINFUNCTYPE(ctypes.HRESULT, ctypes.c_void_p, *argtypes)
    return prototype(vtable[slot])


def _win32_release(interface):
    if interface:
        _win32_com_method(interface, _SLOT_RELEASE)(interface)


def _win32_read_link(path):
    ctypes = _win32()
    ole32 = ctypes.WinDLL("ole32", use_last_error=True)
    ole32.CLSIDFromString.argtypes = (ctypes.c_wchar_p, ctypes.c_void_p)
    ole32.CLSIDFromString.restype = ctypes.HRESULT
    ole32.CoInitializeEx.argtypes = (ctypes.c_void_p, ctypes.c_uint32)
    ole32.CoInitializeEx.restype = ctypes.c_long
    ole32.CoCreateInstance.argtypes = (
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_void_p,
    )
    ole32.CoCreateInstance.restype = ctypes.HRESULT

    initialized = ole32.CoInitializeEx(None, _COINIT_APARTMENTTHREADED)
    # A thread already in the other apartment model is left exactly as found:
    # uninitialising someone else's apartment on the way out would break the
    # code that entered it.
    entered = initialized >= 0

    link = ctypes.c_void_p()
    persist = ctypes.c_void_p()
    try:
        if initialized < 0 and initialized != _RPC_E_CHANGED_MODE:
            raise UnavailableError(
                "read_link", "CoInitializeEx failed with 0x{:08X}".format(initialized & 0xFFFFFFFF)
            )

        clsid = _win32_guid(ole32, "read_link", _CLSID_SHELL_LINK)
        iid_link = _win32_guid(ole32, "read_link", _IID_ISHELLLINKW)
        iid_persist = _win32_guid(ole32, "read_link", _IID_IPERSISTFILE)

        if ole32.CoCreateInstance(
            ctypes.byref(clsid),
            None,
            _CLSCTX_INPROC_SERVER,
            ctypes.byref(iid_link),
            ctypes.byref(link),
        ) != 0:
            raise UnavailableError(
                "read_link", "the shell link class could not be created"
            )

        query = _win32_com_method(
            link, _SLOT_QUERY_INTERFACE, ctypes.c_void_p, ctypes.c_void_p
        )
        if query(link, ctypes.byref(iid_persist), ctypes.byref(persist)) != 0:
            raise UnavailableError(
                "read_link", "the shell link does not implement IPersistFile"
            )

        load = _win32_com_method(
            persist, _SLOT_PERSIST_FILE_LOAD, ctypes.c_wchar_p, ctypes.c_uint32
        )
        if load(persist, ctypes.c_wchar_p(os.path.abspath(path)), _STGM_READ) != 0:
            raise UnavailableError(
                "read_link", "{!r} could not be loaded as a shortcut".format(path)
            )

        buffer = ctypes.create_unicode_buffer(_MAX_PATH)
        get_path = _win32_com_method(
            link,
            _SLOT_SHELL_LINK_GET_PATH,
            ctypes.c_wchar_p,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint32,
        )
        # A shortcut to a virtual shell folder — Control Panel, This PC — has
        # no filesystem path at all. `GetPath` succeeds and leaves the buffer
        # empty; reporting "" as a path would be a lie, so it is refused.
        if get_path(link, buffer, _MAX_PATH, None, _SLGP_RAWPATH) < 0 or not buffer.value:
            raise UnavailableError(
                "read_link",
                "{!r} names no filesystem target; it points at a virtual "
                "shell folder".format(path),
            )
        return buffer.value
    finally:
        _win32_release(persist)
        _win32_release(link)
        if entered:
            ole32.CoUninitialize()


def _win32_known_folder_path(guid):
    ctypes = _win32()
    ole32 = ctypes.WinDLL("ole32", use_last_error=True)
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    ole32.CLSIDFromString.argtypes = (ctypes.c_wchar_p, ctypes.c_void_p)
    ole32.CLSIDFromString.restype = ctypes.HRESULT
    ole32.CoTaskMemFree.argtypes = (ctypes.c_void_p,)
    ole32.CoTaskMemFree.restype = None
    shell32.SHGetKnownFolderPath.argtypes = (
        ctypes.c_void_p,
        ctypes.c_uint32,
        ctypes.c_void_p,
        ctypes.c_void_p,
    )
    shell32.SHGetKnownFolderPath.restype = ctypes.HRESULT

    folder = _win32_guid(ole32, "shell_known_folder_path", guid)
    out = ctypes.c_wchar_p()
    try:
        if shell32.SHGetKnownFolderPath(
            ctypes.byref(folder), 0, None, ctypes.byref(out)
        ) != 0:
            raise UnavailableError(
                "shell_known_folder_path",
                "the shell does not know a folder with GUID {}".format(guid),
            )
        return out.value
    finally:
        # The shell allocates with the COM task allocator, so the caller frees
        # with it; `ctypes` does not own this string.
        if out:
            ole32.CoTaskMemFree(out)


# --------------------------------------------------------------------------
# The undocumented-internal guard (spec 14.12)
# --------------------------------------------------------------------------


def __getattr__(name):
    """Turns a reach for an undelivered helper into an attributable report.

    ``fuzzy_score`` remains outside the layer: spec 14.12 exempts exact
    reproduction of undocumented ranking behaviour, so reproducing it would be
    guessing at a number other people's results are ordered by. A plugin
    reaching for it gets the same attributable diagnostic as one reaching for
    a private internal, rather than an ``AttributeError`` from nowhere in
    particular.
    """
    if name.startswith("__") and name.endswith("__"):
        raise AttributeError(name)
    raise _keypirinha.UndocumentedApiError("keypirinha_util", name)
