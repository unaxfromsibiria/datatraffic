import asyncio
import os
from pathlib import Path

import click

from ._lib import FFIError
from ._lib import generate_key
from .api import AsyncClient


def resolve_settings(redis=None, key=None, client=None, password=None):
    """Resolve final settings from CLI values with env fallback (CLI > env)."""
    return {
        "redis_url": redis or os.environ.get("REDIS_URL"),
        "key_hex": key or os.environ.get("CRYPTO_KEY"),
        "client_name": client or os.environ.get("CLIENT_NAME"),
        "password": password or os.environ.get("REDIS_PASSWORD"),
    }


def _make_client(resolved, watch_projects=()):
    return AsyncClient(
        resolved["redis_url"],
        resolved["key_hex"],
        resolved.get("client_name") or "",
        resolved.get("password") or "",
        watch_projects=watch_projects,
    )


def _save_file(out_dir, file):
    d = Path(out_dir)
    d.mkdir(parents=True, exist_ok=True)
    path = d / file.name
    path.write_bytes(file.data)
    click.echo("saved to %s" % path, err=True)


async def run_server(resolved, listen, out_dir):
    """Subscribe to project topics and save received files until stopped."""
    who = resolved.get("client_name") or "<auto>"
    click.echo(
        "listening on %s as '%s' (Ctrl+C to stop)" % (", ".join(listen), who),
        err=True,
    )
    async with _make_client(resolved, listen) as client:
        while True:
            f = await client.recv()
            click.echo(
                "received '%s' (%d bytes) on project '%s' (note: '%s')"
                % (f.name, len(f.data), f.project, f.note),
                err=True,
            )
            if out_dir:
                _save_file(out_dir, f)


async def run_client(resolved, send_file, project, msg):
    """Read a file and publish it as a DataFile on the target topic."""
    path = Path(send_file)
    data = path.read_bytes()
    name = path.name
    async with _make_client(resolved, watch_projects=()) as client:
        click.echo(
            "sending '%s' (%d bytes) to project '%s'"
            % (name, len(data), project),
            err=True,
        )
        client.send(project, name, msg or "", data)
        await client.flush()  # wait for the hand-off to the transport before shutdown
    click.echo("done", err=True)


@click.command()
@click.option("--redis", default=None, help="Redis URL; overrides REDIS_URL.")
@click.option(
    "--key", "key_hex", default=None,
    help="AES key hex (64 chars); overrides CRYPTO_KEY.",
)
@click.option(
    "--password", default=None,
    help="Redis password; overrides REDIS_PASSWORD.",
)
@click.option(
    "--client", "client_name", default=None,
    help="Client name; overrides CLIENT_NAME or random.",
)
@click.option(
    "--listen", multiple=True, default=(),
    help="Project topic to subscribe; repeat for multiple (server mode).",
)
@click.option(
    "--out-dir", type=click.Path(), default=None,
    help="Directory for received files.",
)
@click.option(
    "--send-file", type=click.Path(), default=None,
    help="File to send (client mode).",
)
@click.option("--msg", default="", help="Note attached to the sent payload.")
@click.option(
    "--project", default=None,
    help="Target project topic (client mode).",
)
@click.option(
    "--genkey", is_flag=True,
    help="Print a random 32-byte key in hex and exit.",
)
def main(
    redis,
    key_hex,
    password,
    client_name,
    listen,
    out_dir,
    send_file,
    msg,
    project,
    genkey,
):
    """Send and receive encrypted files between projects via Redis pub/sub."""
    if genkey:
        click.echo(generate_key())
        return

    resolved = resolve_settings(redis, key_hex, client_name, password)
    if not resolved["redis_url"]:
        raise click.UsageError("Redis URL is required (--redis or REDIS_URL)")
    if not resolved["key_hex"]:
        raise click.UsageError("Crypto key is required (--key or CRYPTO_KEY)")

    try:
        if listen:
            asyncio.run(run_server(resolved, list(listen), out_dir))
        else:
            if not (send_file or project):
                click.echo(click.get_current_context().get_help())
                raise SystemExit(1)
            if not send_file or not project:
                raise click.UsageError(
                    "client mode requires both --send-file <path> and --project <name>"
                )
            asyncio.run(run_client(resolved, send_file, project, msg))
    except FFIError as e:
        click.echo("Error: %s" % e, err=True)
        raise SystemExit(1)
    except (KeyboardInterrupt, asyncio.CancelledError):
        # Ctrl+C: the async context manager already released the client.
        click.echo("stopped", err=True)


if __name__ == "__main__":
    main()
