import asyncio
import os
from concurrent.futures import ThreadPoolExecutor

from ._lib import Client
from ._lib import ReceivedFile


def _pool_size():
    """FFI worker count: 2 on a multi-core host, otherwise 1."""
    return 2 if (os.cpu_count() or 0) > 1 else 1


class AsyncClient:
    """Non-blocking wrapper around :class:`_lib.Client`.

    Background tasks pump sends and receives; blocking FFI calls run in a small
    thread pool so the event loop is never held up by a recv timeout::

        async with AsyncClient(redis_url, key_hex) as c:
            c.send("demo", "a.txt", "note", b"data")
            f = await c.recv()
    """

    def __init__(
        self,
        redis_url,
        key_hex,
        client_name="",
        password="",
        watch_projects=(),
    ):
        self._client = Client(
            redis_url,
            key_hex,
            client_name,
            password,
            watch_projects=watch_projects,
        )
        # FFI entry points must run sequentially on one handle; the pool offloads
        # them from the event loop and _ffi_lock keeps at most one call in flight.
        self._pool = ThreadPoolExecutor(max_workers=_pool_size())
        self._ffi_lock = asyncio.Lock()
        self._inbox: asyncio.Queue[ReceivedFile] = asyncio.Queue()
        self._outbox: asyncio.Queue[tuple[str, str, str, bytes] | None] = asyncio.Queue()
        self._send_task: asyncio.Task | None = None
        self._recv_task: asyncio.Task | None = None
        self._closed = False

    async def _ffi(self, fn, *args):
        """Run one blocking FFI call in the pool; only one runs at a time."""
        async with self._ffi_lock:
            loop = asyncio.get_running_loop()
            return await loop.run_in_executor(self._pool, fn, *args)

    async def _send_loop(self):
        """Drain the outbox and publish each file until closed."""
        while not self._closed:
            item = await self._outbox.get()
            try:
                if item is None:  # shutdown sentinel from close()
                    break
                project, name, note, data = item
                await self._ffi(self._client.send, project, name, note, data)
            finally:
                self._outbox.task_done()

    async def _recv_loop(self):
        """Poll the client in a worker thread and push received files into the inbox."""
        while not self._closed:
            # 10 ms poll: recv runs off the event loop, so a short window only trims
            # delivery latency and how long it holds _ffi_lock (which send waits on).
            file = await self._ffi(self._client.recv, 10)
            if file is not None:
                self._inbox.put_nowait(file)

    async def recv(self) -> ReceivedFile:
        """Wait for and return the next received file."""
        return await self._inbox.get()

    def send(self, project: str, name: str, note: str = "", data: bytes = b""):
        """Queue a file for sending to the given project; the background task publishes it."""
        self._outbox.put_nowait((project, name, note, data))

    async def flush(self):
        """Wait until every queued send has been handed to the transport (no-op if empty)."""
        await self._outbox.join()

    async def close(self):
        """Stop both tasks and release the FFI handle (idempotent)."""
        if self._closed:
            return
        self._closed = True
        # Sentinel unblocks the send loop if it is waiting on an empty outbox.
        self._outbox.put_nowait(None)
        tasks = [t for t in (self._send_task, self._recv_task) if t is not None]
        await asyncio.gather(*tasks, return_exceptions=True)
        # Loops exited normally, so no FFI call is in flight; release handle and pool.
        self._client.close()
        self._pool.shutdown(wait=True)

    async def __aenter__(self):
        self._send_task = asyncio.create_task(self._send_loop())
        self._recv_task = asyncio.create_task(self._recv_loop())
        return self

    async def __aexit__(self, exc_type, exc, tb):
        await self.close()
