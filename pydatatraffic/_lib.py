"""ctypes bindings for the libdatatraffic C ABI (cdylib).

The shared library loads lazily on first use, so importing this module never
requires a prebuilt .so. Call all FFI entry points sequentially from one thread
(the Rust side drives an async client on a single global runtime).
"""

import ctypes
import os
import platform
import secrets
from ctypes import POINTER
from ctypes import c_uint8
from ctypes import c_void_p
from dataclasses import dataclass
from pathlib import Path

# Error codes mirrored from libdatatraffic::lib.
FFI_OK = 0
FFI_ERR_BAD_INPUT = 1
FFI_ERR_CLIENT_CREATE = 2
FFI_ERR_SEND = 3

_ERROR_MESSAGES = {
    FFI_OK: "ok",
    FFI_ERR_BAD_INPUT: "bad input (null or invalid argument)",
    FFI_ERR_CLIENT_CREATE: "failed to create traffic client",
    FFI_ERR_SEND: "send failed (outbox closed)",
}


class FFIError(Exception):
    """Raised when an FFI call reports a non-OK error code."""

    def __init__(self, code):
        self.code = code
        super().__init__("ffi error %d: %s" % (code, _describe(code)))


def _describe(code):
    return _ERROR_MESSAGES.get(code, "unknown error")


@dataclass
class ReceivedFile:
    """A data file received from the wire."""

    project: str
    name: str
    note: str
    data: bytes


# --- ctypes structures (must match #[repr(C)] layouts in ffi.rs) -------------


class _FfiBuffer(ctypes.Structure):
    _fields_ = [
        ("ptr", POINTER(c_uint8)),
        ("len", ctypes.c_size_t),
        ("cap", ctypes.c_size_t),
    ]


class _FfiDataFile(ctypes.Structure):
    _fields_ = [
        ("project", _FfiBuffer),
        ("name", _FfiBuffer),
        ("note", _FfiBuffer),
        ("data", _FfiBuffer),
    ]


_LIB_PREFIX = "libdatatraffic.so.1."
# Map platform.machine() values to the cargo target-arch names used in the .so
# filename suffix (CARGO_CFG_TARGET_ARCH / rustc host).
_ARCH_ALIASES = {
    "amd64": "x86_64",
    "arm64": "aarch64",
}


def _target_arch():
    """Current machine arch normalized to the cargo target-arch name."""
    machine = platform.machine().lower()
    return _ARCH_ALIASES.get(machine, machine)


def _lib_name():
    """Shared library filename for the current architecture."""
    return _LIB_PREFIX + _target_arch()


def _lib_arch(path):
    """Arch suffix encoded in a library filename, or None if absent."""
    name = Path(path).name
    if name.startswith(_LIB_PREFIX) and len(name) > len(_LIB_PREFIX):
        return name[len(_LIB_PREFIX):]
    return None


_lib = None


def _candidate_paths():
    """Ordered candidate locations to search for the shared library."""
    lib_name = _lib_name()
    paths = []
    override = os.environ.get("LIBDATATRAFFIC_LIB")
    if override:
        paths.append(Path(override))
    paths.append(Path(__file__).resolve().parent / lib_name)
    paths.append(Path.cwd() / lib_name)
    for d in ("/usr/local/lib", "/usr/lib"):
        paths.append(Path(d) / lib_name)
    ld = os.environ.get("LD_LIBRARY_PATH")
    if ld:
        for entry in ld.split(":"):
            if entry:
                paths.append(Path(entry) / lib_name)
    return paths


def _find_lib():
    """Return the first existing candidate path, else raise FileNotFoundError."""
    candidates = _candidate_paths()
    for p in candidates:
        if p.exists():
            return p
    checked = "\n  ".join(str(p) for p in candidates)
    raise FileNotFoundError(
        "shared library %s not found; run 'make build-lib' first. Checked:\n  %s"
        % (_lib_name(), checked)
    )


def _get_lib():
    """Load and configure the shared library once (lazy)."""
    global _lib
    if _lib is not None:
        return _lib
    lib_path = _find_lib()
    file_arch = _lib_arch(lib_path)
    cur_arch = _target_arch()
    if file_arch and file_arch != cur_arch:
        raise RuntimeError(
            "%s built for %s but running on %s; run 'cargo build -p "
            "libdatatraffic' in the project root (see README.md)"
            % (lib_path.name, file_arch, cur_arch)
        )
    lib = ctypes.CDLL(str(lib_path))

    lib.ffi_last_error.restype = ctypes.c_int32

    lib.ffi_client_new.argtypes = [c_void_p, ctypes.c_size_t] * 5
    lib.ffi_client_new.restype = c_void_p

    lib.ffi_client_send.argtypes = [
        c_void_p,
        c_void_p,
        ctypes.c_size_t,
        c_void_p,
        ctypes.c_size_t,
        c_void_p,
        ctypes.c_size_t,
        c_void_p,
        ctypes.c_size_t,
    ]
    lib.ffi_client_send.restype = ctypes.c_int32

    lib.ffi_client_recv.argtypes = [c_void_p, ctypes.c_uint32]
    lib.ffi_client_recv.restype = POINTER(_FfiDataFile)

    lib.ffi_datafile_free.argtypes = [POINTER(_FfiDataFile)]
    lib.ffi_buffer_free.argtypes = [c_void_p, ctypes.c_size_t, ctypes.c_size_t]
    lib.ffi_client_free.argtypes = [c_void_p]

    _lib = lib
    return lib


def _bytes(value):
    """Byte value -> (ctypes array or None, length); empty -> null/0."""
    if value is None:
        return None, 0
    data = value.encode("utf-8") if isinstance(value, str) else bytes(value)
    if not data:
        return None, 0
    arr = (c_uint8 * len(data))(*data)
    return arr, len(data)


def _read_buf(buf):
    """Copy an FfiBuffer into Python bytes."""
    if not buf.ptr or buf.len == 0:
        return b""
    return ctypes.string_at(buf.ptr, buf.len)


class Client:
    """Synchronous wrapper over the traffic client lifecycle.

    Use as a context manager so the handle is always released::

        with Client(redis_url, key_hex) as c:
            f = c.recv(timeout_ms=1000)
    """

    def __init__(
        self,
        redis_url,
        key_hex,
        client_name="",
        password="",
        watch_projects=(),
    ):
        lib = _get_lib()
        ru, rul = _bytes(redis_url)
        kh, khl = _bytes(key_hex)
        cn, cnl = _bytes(client_name)
        pw, pwl = _bytes(password)
        wp, wpl = _bytes(";".join(watch_projects))
        handle = lib.ffi_client_new(
            ru, rul, kh, khl, cn, cnl, pw, pwl, wp, wpl
        )
        if not handle:
            raise FFIError(lib.ffi_last_error())
        self._lib = lib
        self._handle = handle

    def send(self, project, name, note="", data=b""):
        """Send a data file to the given project; raises FFIError on failure."""
        pr, prl = _bytes(project)
        n, nl = _bytes(name)
        no, nol = _bytes(note)
        d, dl = _bytes(data)
        code = self._lib.ffi_client_send(
            self._handle, pr, prl, n, nl, no, nol, d, dl
        )
        if code != FFI_OK:
            raise FFIError(code)

    def recv(self, timeout_ms=1000):
        """Receive a file, waiting up to ``timeout_ms``; None on timeout."""
        ptr = self._lib.ffi_client_recv(
            self._handle, ctypes.c_uint32(timeout_ms)
        )
        if not ptr:
            return None
        f = ptr.contents
        project = _read_buf(f.project).decode("utf-8", "replace")
        name = _read_buf(f.name).decode("utf-8", "replace")
        note = _read_buf(f.note).decode("utf-8", "replace")
        data = _read_buf(f.data)
        self._lib.ffi_datafile_free(ptr)
        return ReceivedFile(project=project, name=name, note=note, data=data)

    def close(self):
        """Release the client handle (idempotent)."""
        if self._handle is not None:
            self._lib.ffi_client_free(self._handle)
            self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()
        return False


def generate_key():
    """Return a random 32-byte AES-256-GCM key as a 64-char hex string."""
    return secrets.token_hex(32)
