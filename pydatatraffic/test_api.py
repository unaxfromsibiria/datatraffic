import asyncio

import pytest


def _hex64():
    return "ab" * 32


class _FakeFile:
    """Minimal stand-in with the same shape as ReceivedFile."""

    def __init__(self, project, name, note, data):
        self.project = project
        self.name = name
        self.note = note
        self.data = data


class FakeClient:
    """Stand-in for _lib.Client: records close and echoes sent files back."""

    def __init__(self, *args, **kwargs):
        self.closed = False
        self._pending = []

    def send(self, project, name, note="", data=b""):
        self._pending.append(_FakeFile(project, name, note, data))

    def recv(self, timeout_ms=1000):
        if self._pending:
            return self._pending.pop(0)
        return None

    def close(self):
        self.closed = True


def _install_fake(monkeypatch):
    from pydatatraffic import api

    monkeypatch.setattr(api, "Client", FakeClient)


# --- smoke -------------------------------------------------------------------


def test_smoke_import_api():
    from pydatatraffic import api

    assert hasattr(api, "AsyncClient")


# --- constructor error -------------------------------------------------------


def test_constructor_invalid_key_raises_ffi_error():
    # A 4-char hex key is 2 bytes, not the 32 required by AES-256-GCM. The FFI
    # rejects it in Settings::resolve before any Redis connection is attempted.
    from pydatatraffic._lib import FFIError
    from pydatatraffic.api import AsyncClient

    with pytest.raises(FFIError):
        AsyncClient("redis://localhost:6379", "abcd")


# --- close idempotency -------------------------------------------------------


def test_close_is_idempotent(monkeypatch):
    from pydatatraffic.api import AsyncClient

    _install_fake(monkeypatch)

    async def scenario():
        c = AsyncClient("redis://x", _hex64())
        await c.close()
        await c.close()  # second call must not raise
        return c._client.closed

    assert asyncio.run(scenario()) is True


# --- context manager releases the handle -------------------------------------


def test_context_manager_releases_handle(monkeypatch):
    from pydatatraffic.api import AsyncClient

    _install_fake(monkeypatch)

    async def scenario():
        async with AsyncClient("redis://x", _hex64()) as c:
            assert c._send_task is not None
            assert c._recv_task is not None
        return c._client.closed, c._closed

    closed, was_closed = asyncio.run(scenario())
    assert closed is True
    assert was_closed is True


# --- send/recv plumbing through the wrapper ----------------------------------


def test_send_recv_roundtrip_through_wrapper(monkeypatch):
    from pydatatraffic.api import AsyncClient

    _install_fake(monkeypatch)

    async def scenario():
        async with AsyncClient("redis://x", _hex64()) as c:
            c.send("demo", "a.txt", "note", b"payload")
            f = await asyncio.wait_for(c.recv(), timeout=2.0)
            return (f.project, f.name, f.note, f.data)

    # The project travels with the file end-to-end through the wrapper.
    assert asyncio.run(scenario()) == ("demo", "a.txt", "note", b"payload")


# --- flush guarantees the send hand-off --------------------------------------


def test_flush_waits_for_send_handoff(monkeypatch):
    from pydatatraffic import api as _api
    from pydatatraffic.api import AsyncClient

    sent = []

    class CountingFake(FakeClient):
        # Record each hand-off to the transport; recv() may drain _pending in
        # parallel, so counting send calls is the deterministic signal.
        def send(self, project, name, note="", data=b""):
            sent.append((project, name))
            super().send(project, name, note, data)

    monkeypatch.setattr(_api, "Client", CountingFake)

    async def scenario():
        async with AsyncClient("redis://x", _hex64()) as c:
            c.send("demo", "a.txt", "note", b"payload")
            await c.flush()  # must not return until the send task handed off
            return list(sent)

    # After flush, exactly one hand-off to the transport has occurred.
    assert asyncio.run(scenario()) == [("demo", "a.txt")]


# --- FFI thread pool sizing --------------------------------------------------


def test_pool_size_is_two_on_multicore(monkeypatch):
    from pydatatraffic import api

    monkeypatch.setattr(api.os, "cpu_count", lambda: 4)
    assert api._pool_size() == 2


def test_pool_size_is_one_on_single_core(monkeypatch):
    from pydatatraffic import api

    monkeypatch.setattr(api.os, "cpu_count", lambda: 1)
    assert api._pool_size() == 1


def test_pool_size_is_one_when_cpu_unknown(monkeypatch):
    from pydatatraffic import api

    monkeypatch.setattr(api.os, "cpu_count", lambda: None)
    assert api._pool_size() == 1
