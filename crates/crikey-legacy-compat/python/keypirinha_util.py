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
import fnmatch
import os
import shutil
import subprocess
import sys

import keypirinha as _keypirinha

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

#: Characters that force :func:`cmdline_quote` to quote an argument. A bare
#: backslash is *not* one of them: `C:\\dir` needs no quoting and quoting it
#: would break plugins that compare the result against a literal.
_MUST_QUOTE = (" ", "\t", '"')

_BACKSLASH = "\\"
_QUOTE = '"'


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


def cmdline_quote(argument):
    """The inverse of :func:`cmdline_split`.

    Accepts one argument or a sequence of them; a sequence is joined with
    single spaces. ``cmdline_split(cmdline_quote(args)) == args`` holds for
    every argument vector, including empty strings, embedded quotes and
    trailing backslashes — that round trip, not the exact spelling, is the
    contract.

    Quotes are added only when they are *needed*, because a plugin that
    compares a built command line against a literal would break otherwise.
    """
    if not isinstance(argument, str):
        return " ".join(cmdline_quote(item) for item in argument)

    if argument and not any(char in argument for char in _MUST_QUOTE):
        return argument

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
    """What :func:`scan_directory` should report and how far it should walk."""

    FILES = 0x1
    FOLDERS = 0x2

    #: Descend below the base directory. Bounded by ``max_level``.
    RECURSIVE = 0x4

    #: Everything directly inside the base directory.
    DEFAULT = 0x3


def scan_directory(
    base_dir,
    name_patterns="*",
    flags=ScanFlags.DEFAULT,
    max_level=-1,
    max_entries=_MAX_SCAN_RESULTS,
):
    """Lists entries under `base_dir` matching `name_patterns`.

    `name_patterns` is one glob or a sequence of them, matched against the
    entry *name* only — never against the whole path, so ``*.txt`` finds
    ``sub/deep/notes.txt`` on a recursive scan.

    Results are **relative to `base_dir`**, joined with the host path
    separator, and sorted. Relative because a plugin joins them straight back
    onto the base it passed in; sorted because a plugin's own output is a
    catalog, and an unordered catalog reorders itself between runs for no
    reason the user can see.

    `max_level` bounds recursion: ``-1`` is unlimited, ``0`` never descends,
    and ``n`` descends into directories at most ``n`` levels below the base.
    Entries directly inside `base_dir` are level 0, so ``max_level=1`` reaches
    ``sub/child`` but not ``sub/deep/child``.

    A directory that cannot be read is skipped rather than fatal: a plugin
    scanning a tree it does not fully own must still get the part it can see.

    At most `max_entries` entries are retained. On overflow the scan stops and
    one line naming the truncation is written to the plugin log channel — a
    truncated listing that says so beats an out-of-memory kill of the worker,
    and beats a truncated listing that lies.
    """
    patterns = (
        (name_patterns,) if isinstance(name_patterns, str) else tuple(name_patterns)
    )
    if not patterns:
        patterns = ("*",)

    want_files = bool(flags & ScanFlags.FILES)
    want_folders = bool(flags & ScanFlags.FOLDERS)
    recursive = bool(flags & ScanFlags.RECURSIVE)

    results = []
    truncated = False
    pending = [("", 0)]

    while pending and not truncated:
        relative_dir, depth = pending.pop()
        absolute = os.path.join(base_dir, relative_dir) if relative_dir else base_dir
        try:
            with os.scandir(absolute) as entries:
                children = sorted((entry.name, entry.is_dir()) for entry in entries)
        except OSError:
            continue

        for name, is_dir in children:
            relative = os.path.join(relative_dir, name) if relative_dir else name

            if (is_dir and want_folders) or (not is_dir and want_files):
                if any(fnmatch.fnmatch(name, pattern) for pattern in patterns):
                    if len(results) >= max_entries:
                        truncated = True
                        break
                    results.append(relative)

            if is_dir and recursive and (max_level < 0 or depth < max_level):
                pending.append((relative, depth + 1))

    if truncated:
        sys.stderr.write(
            "[warn][keypirinha_util] scan_directory({!r}) stopped at the {} entry cap; "
            "the returned list is truncated\n".format(base_dir, max_entries)
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
    ``except OSError`` misses and its bare ``except`` misattributes.
    """
    try:
        completed = subprocess.run(
            argv,
            input=stdin_text,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
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
    import ctypes

    return ctypes


def _win32_set_clipboard(text):
    ctypes = _win32()
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    if not user32.OpenClipboard(None):
        raise UnavailableError(
            "set_clipboard", "the clipboard is locked by another process"
        )
    try:
        user32.EmptyClipboard()
        size = (len(text) + 1) * ctypes.sizeof(ctypes.c_wchar)
        handle = kernel32.GlobalAlloc(_GMEM_MOVEABLE, ctypes.c_size_t(size))
        if not handle:
            raise UnavailableError("set_clipboard", "the clipboard allocation failed")
        buffer = kernel32.GlobalLock(handle)
        try:
            ctypes.memmove(buffer, ctypes.create_unicode_buffer(text), size)
        finally:
            kernel32.GlobalUnlock(handle)
        if not user32.SetClipboardData(_CF_UNICODETEXT, handle):
            raise UnavailableError("set_clipboard", "SetClipboardData refused the text")
    finally:
        user32.CloseClipboard()


def _win32_get_clipboard():
    ctypes = _win32()
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

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
        try:
            return ctypes.c_wchar_p(buffer).value or ""
        finally:
            kernel32.GlobalUnlock(handle)
    finally:
        user32.CloseClipboard()


def _win32_shell_execute(operation, target, args, working_dir, verb):
    ctypes = _win32()
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    result = shell32.ShellExecuteW(
        None,
        ctypes.c_wchar_p(verb or None),
        ctypes.c_wchar_p(target),
        ctypes.c_wchar_p(args or None),
        ctypes.c_wchar_p(working_dir or None),
        _SW_SHOWNORMAL,
    )
    if result <= _SE_ERROR_CEILING:
        raise UnavailableError(
            operation, "ShellExecuteW failed with code {}".format(result)
        )


def _win32_explore_file(path):
    ctypes = _win32()
    if os.path.isdir(path):
        _win32_shell_execute("explore_file", path, "", "", "explore")
        return
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
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
    if result <= _SE_ERROR_CEILING:
        raise UnavailableError(
            "explore_file", "ShellExecuteW failed with code {}".format(result)
        )


# --------------------------------------------------------------------------
# The undocumented-internal guard (spec 14.12)
# --------------------------------------------------------------------------


def __getattr__(name):
    """Turns a reach for an undelivered helper into an attributable report.

    Several documented Keypirinha helpers are deliberately outside M3
    (``fuzzy_score``, ``chardet_open``, ``decode_bytes``, ``kwargs_encode``,
    ``kwargs_decode``, ``execute_default_action``, ``web_browser_command``,
    ``read_link``). A plugin reaching for one gets the same attributable
    diagnostic as one reaching for a private internal, rather than an
    ``AttributeError`` from nowhere in particular.
    """
    if name.startswith("__") and name.endswith("__"):
        raise AttributeError(name)
    raise _keypirinha.UndocumentedApiError("keypirinha_util", name)
