import asyncio

from ._lib import Client
from ._lib import ReceivedFile


class AsyncClient:
    """Non-blocking wrapper around :class:`_lib.Client`.

    Background tasks pump sends and receives::

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
        self._inbox: asyncio.Queue[ReceivedFile] = asyncio.Queue()
        self._outbox: asyncio.Queue[tuple[str, str, str, bytes]] = asyncio.Queue()
        self._send_task: asyncio.Task | None = None
        self._recv_task: asyncio.Task | None = None
        self._closed = False

    async def _send_loop(self):
        """Drain the outbox and publish each file until closed."""
        while not self._closed:
            project, name, note, data = await self._outbox.get()
            try:
                self._client.send(project, name, note, data)
            finally:
                self._outbox.task_done()

    async def _recv_loop(self):
        """Poll the client and push received files into the inbox."""
        while not self._closed:
            file = self._client.recv(50)
            if file is not None:
                self._inbox.put_nowait(file)
            await asyncio.sleep(0)

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
        tasks = [t for t in (self._send_task, self._recv_task) if t is not None]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        self._client.close()

    async def __aenter__(self):
        self._send_task = asyncio.create_task(self._send_loop())
        self._recv_task = asyncio.create_task(self._recv_loop())
        return self

    async def __aexit__(self, exc_type, exc, tb):
        await self.close()
