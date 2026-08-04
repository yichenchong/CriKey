"""Documented ``keypirinha_wintypes`` module of the Legacy Compatibility Layer.

The documented Win32 interop module (spec 14.2). Everything it exposes is
backed by a Windows DLL, so on any other platform there is nothing behind it —
and the way it says so is the entire contract (spec 14.10, 14.12; acceptance
31.31).

Three properties, in order of how easy they are to get wrong:

1. **It imports successfully everywhere.** A windows-only module that raised on
   import would break plugin *loading* on Linux, before the layer ever got to
   classify the package. The plugin would be reported unloadable, which is
   false: it loads fine and merely cannot exercise one branch. So the import is
   unconditional and cheap, and nothing Windows-specific is touched here.

2. **Every Win32-backed name refuses loudly off Windows.** Attribute access
   raises :class:`WindowsOnlyError`, naming the symbol, the platform and the
   windows-only nature of the dependency. Not a stub that returns ``None``, not
   a mock that pretends: a usable-looking stub turns "we could not check this"
   into a green tick, which is exactly the plausible lie a compatibility report
   exists to prevent.

3. **The refusal cannot be laundered into silence.**
   :class:`WindowsOnlyError` is a ``RuntimeError`` and deliberately **not** an
   ``AttributeError``, so ``hasattr(kpwt, "kernel32")`` *propagates* instead of
   answering ``False``, and ``getattr(kpwt, "kernel32", None)`` does not hand
   back a quiet ``None``. A plugin therefore cannot probe its way past the
   classification, and the layer always sees the access.

The honest guard, and the only one that works, is :func:`is_available`::

    import keypirinha_wintypes as kpwt

    if not kpwt.is_available():
        return "unavailable"
    metric = kpwt.declare_func(kpwt.user32, "GetSystemMetrics",
                               ret=ctypes.c_int, arg=[ctypes.c_int])
"""

import sys
import ctypes

import uuid
import keypirinha as _keypirinha

__all__ = (
    "WINDOWS_ONLY",
    "WINDOWS_ONLY_SYMBOLS",
    "WindowsOnlyError",
    "is_available",
)

#: This module is classified windows-only on every platform, Windows included.
#: A constant rather than a computed value: the *classification* of the module
#: does not change with the host, only whether its symbols can resolve, and a
#: plugin importing it is never portable regardless of where it is inspected.
WINDOWS_ONLY = True

#: Every name in this module that is backed by Win32 and therefore cannot
#: resolve off Windows.
#:
#: Enumerated rather than discovered so the diagnostics layer and the API
#: compatibility matrix can report on the surface without importing Windows
#: machinery, and so trimming the tuple is a visible change to a contract
#: instead of a quiet way to make a report look better.
WINDOWS_ONLY_SYMBOLS = (
    "kernel32",
    "user32",
    "shell32",
    "ole32",
    "declare_func",
    "GUID",
)


class WindowsOnlyError(_keypirinha.KeypirinhaError, RuntimeError):
    """A Win32-backed name was reached on a platform that has no Win32.

    ``RuntimeError`` and pointedly not ``AttributeError``: ``hasattr`` and
    ``getattr(..., default)`` swallow ``AttributeError`` by definition, and a
    swallowed refusal here would let a plugin's probe report "no Win32 needed"
    for a plugin that needs Win32.

    Rooted in :class:`keypirinha.KeypirinhaError` so the layer has one error
    taxonomy to report on (spec 26.2), and carries :attr:`symbol` and
    :attr:`platform` beside the message so the diagnostics layer classifies
    without parsing English.
    """

    def __init__(self, symbol, platform=None):
        self.symbol = symbol
        self.platform = platform if platform is not None else sys.platform
        _keypirinha.KeypirinhaError.__init__(
            self,
            "keypirinha_wintypes.{} is Windows-only and cannot resolve on platform "
            "{}: it is backed by a Win32 DLL that does not exist here".format(
                symbol, self.platform
            ),
        )


def is_available():
    """Whether the Win32-backed names in this module can resolve.

    ``True`` only on Windows. This is the documented probe a plugin guards its
    Win32 branch with, and the only one that works — see the module docstring
    for why ``hasattr`` and ``getattr(..., default)`` do not.
    """
    return sys.platform.startswith("win")


#: `GUID` field layout, kept beside the structure it describes.
_GUID_FIELDS = ("Data1", "Data2", "Data3", "Data4")


def _resolve(symbol):
    """Builds one Win32-backed symbol. Only ever called on Windows.

    ``ctypes`` itself is part of the standard library on every supported
    platform; only the Windows DLL lookup is deferred until this function.
    """

    if symbol in ("kernel32", "user32", "shell32", "ole32"):
        # `use_last_error` keeps `GetLastError` meaningful across the ctypes
        # call boundary, which is the only way a failed Win32 call can be
        # reported as anything better than "it returned zero".
        return ctypes.WinDLL(symbol, use_last_error=True)

    if symbol == "GUID":

        class GUID(ctypes.Structure):
            """A Win32 ``GUID`` / ``IID``, laid out as the ABI requires."""

            _fields_ = [
                (_GUID_FIELDS[0], ctypes.c_ulong),
                (_GUID_FIELDS[1], ctypes.c_ushort),
                (_GUID_FIELDS[2], ctypes.c_ushort),
                (_GUID_FIELDS[3], ctypes.c_ubyte * 8),
            ]

            def __init__(self, value):
                super().__init__()
                if isinstance(value, GUID):
                    ctypes.memmove(
                        ctypes.addressof(self),
                        ctypes.addressof(value),
                        ctypes.sizeof(self),
                    )
                    return
                parsed = value if isinstance(value, uuid.UUID) else uuid.UUID(str(value))
                self.Data1, self.Data2, self.Data3 = parsed.fields[:3]
                self.Data4[0] = parsed.clock_seq_hi_variant
                self.Data4[1] = parsed.clock_seq_low
                for index in range(2, 8):
                    self.Data4[index] = (parsed.node >> ((7 - index) * 8)) & 0xFF

            def __repr__(self):
                tail = "".join("{:02X}".format(part) for part in self.Data4)
                return "GUID({:08X}-{:04X}-{:04X}-{})".format(
                    self.Data1, self.Data2, self.Data3, tail
                )

        return GUID

    if symbol == "declare_func":
        return _declare_func

    raise WindowsOnlyError(symbol)

def _declare_func(dll, name, ret=None, arg=None, args=None):
    """Binds `name` in `dll` with an explicit ctypes prototype.

    ``args`` is the original Keypirinha keyword; ``arg`` is retained as the
    first M3 shim's spelling. Supplying both is ambiguous and rejected.
    """
    if arg is not None and args is not None:
        raise TypeError("pass either arg or args, not both")
    func = getattr(dll, name)
    func.restype = ret
    argtypes = args if args is not None else arg
    if argtypes is not None:
        func.argtypes = tuple(argtypes)
    return func


def __getattr__(symbol):
    """Resolves a Win32-backed name, or refuses it in a way nothing can hide.

    Reached only when normal module lookup already failed, so the names
    defined above — :data:`WINDOWS_ONLY`, :data:`WINDOWS_ONLY_SYMBOLS`,
    :class:`WindowsOnlyError`, :func:`is_available` — never come through here
    and resolve identically on every platform.

    On Windows a resolved symbol is cached into the module globals, so the DLL
    is loaded once per process rather than once per access.
    """
    if symbol in WINDOWS_ONLY_SYMBOLS:
        if not is_available():
            raise WindowsOnlyError(symbol)
        resolved = _resolve(symbol)
        globals()[symbol] = resolved
        return resolved

    # Python's own protocols probe dunders constantly; answering those with a
    # legacy diagnostic would misattribute `copy`, `pickle` and `inspect`
    # machinery to the plugin.
    if symbol.startswith("__") and symbol.endswith("__"):
        raise AttributeError(symbol)

    raise _keypirinha.UndocumentedApiError("keypirinha_wintypes", symbol)
