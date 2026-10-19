import pytest
from click.testing import CliRunner


def _hex64():
    return "ab" * 32


# --- genkey ------------------------------------------------------------------


def test_genkey_is_64_hex_chars_and_random():
    from pydatatraffic._lib import generate_key

    a = generate_key()
    b = generate_key()
    assert len(a) == 64
    assert all(c in "0123456789abcdef" for c in a)
    assert a != b


# --- settings resolution (CLI > env) -----------------------------------------


def test_resolve_cli_overrides_env(monkeypatch):
    from pydatatraffic.main import resolve_settings

    monkeypatch.setenv("REDIS_URL", "redis://from-env")
    monkeypatch.setenv("CRYPTO_KEY", _hex64())
    monkeypatch.setenv("CLIENT_NAME", "env-client")
    monkeypatch.setenv("REDIS_PASSWORD", "env-pw")

    s = resolve_settings(
        redis="redis://from-cli",
        key=_hex64(),
        client="cli-client",
        password="cli-pw",
    )
    assert s["redis_url"] == "redis://from-cli"
    assert s["client_name"] == "cli-client"
    assert s["password"] == "cli-pw"


def test_resolve_falls_back_to_env(monkeypatch):
    from pydatatraffic.main import resolve_settings

    monkeypatch.setenv("REDIS_URL", "redis://from-env")
    monkeypatch.setenv("CRYPTO_KEY", _hex64())
    monkeypatch.delenv("CLIENT_NAME", raising=False)
    monkeypatch.delenv("REDIS_PASSWORD", raising=False)

    s = resolve_settings()
    assert s["redis_url"] == "redis://from-env"
    assert s["key_hex"] == _hex64()
    assert s["client_name"] is None
    assert s["password"] is None


# --- CLI parsing -------------------------------------------------------------


def test_cli_genkey_flag():
    from pydatatraffic.main import main

    res = CliRunner().invoke(main, ["--genkey"])
    assert res.exit_code == 0, res.output
    out = res.output.strip()
    assert len(out) == 64
    assert all(c in "0123456789abcdef" for c in out)


def test_cli_unknown_flag_rejected():
    from pydatatraffic.main import main

    res = CliRunner().invoke(main, ["--nope"])
    assert res.exit_code != 0


def test_client_mode_requires_both_flags(monkeypatch):
    from pydatatraffic.main import main

    monkeypatch.setenv("REDIS_URL", "redis://x")
    monkeypatch.setenv("CRYPTO_KEY", _hex64())
    # Only --send-file, no --project -> usage error before any client is built.
    res = CliRunner().invoke(main, ["--send-file", "/tmp/whatever.bin"])
    assert res.exit_code == 2
    assert "requires both" in res.output


def test_missing_redis_and_key_errors(monkeypatch):
    from pydatatraffic.main import main

    monkeypatch.delenv("REDIS_URL", raising=False)
    monkeypatch.delenv("CRYPTO_KEY", raising=False)
    args = ["--send-file", "/tmp/x.bin", "--project", "p"]
    res = CliRunner().invoke(main, args)
    assert res.exit_code == 2
    assert "Redis URL is required" in res.output


def test_client_mode_wires_all_flags(monkeypatch, tmp_path):
    from pydatatraffic import main as m

    calls = {}

    def fake_make(resolved, watch_projects=()):
        class C:
            def __init__(self):
                pass

            def send(self, project, name, note="", data=b""):
                calls["sent"] = (project, name, note, data)

            async def flush(self):
                calls["flushed"] = True

            async def __aenter__(self):
                return self

            async def __aexit__(self, *a):
                return False

        calls["resolved"] = resolved
        calls["watch"] = watch_projects
        return C()

    monkeypatch.setattr(m, "_make_client", fake_make)
    f = tmp_path / "hello.txt"
    f.write_bytes(b"payload")

    res = CliRunner().invoke(
        m.main,
        [
            "--redis", "r",
            "--key", _hex64(),
            "--client", "c1",
            "--password", "p",
            "--send-file", str(f),
            "--project", "proj",
            "--msg", "hi",
        ],
    )
    assert res.exit_code == 0, res.output
    assert calls["resolved"]["redis_url"] == "r"
    assert calls["resolved"]["key_hex"] == _hex64()
    assert calls["resolved"]["client_name"] == "c1"
    assert calls["resolved"]["password"] == "p"
    assert calls["watch"] == ()
    # The target project is passed per send, not at client creation.
    assert calls["sent"] == ("proj", "hello.txt", "hi", b"payload")
    # run_client waits for the send hand-off before shutdown (no fixed sleep).
    assert calls.get("flushed") is True


def test_server_mode_parses_listen_and_out_dir(monkeypatch, tmp_path):
    from pydatatraffic import main as m

    calls = {}

    async def fake_run(resolved, listen, out_dir):
        calls["listen"] = listen
        calls["out_dir"] = out_dir

    monkeypatch.setattr(m, "run_server", fake_run)
    res = CliRunner().invoke(
        m.main,
        [
            "--redis", "r",
            "--key", _hex64(),
            "--listen", "alpha",
            "--listen", "beta",
            "--out-dir", str(tmp_path),
        ],
    )
    assert res.exit_code == 0, res.output
    assert calls["listen"] == ["alpha", "beta"]
    assert calls["out_dir"] == str(tmp_path)


# --- FFI wrapper smoke import ------------------------------------------------


def test_smoke_import_lib():
    from pydatatraffic import _lib

    assert hasattr(_lib, "Client")
    assert hasattr(_lib, "generate_key")
    assert _lib.FFI_OK == 0
    assert _lib.FFI_ERR_SEND == 3


# --- _find_lib -----------------------------------------------------------------


def test_find_lib_prefers_package_dir(tmp_path, monkeypatch):
    from pydatatraffic import _lib

    pkg = tmp_path / "pkg"
    cwd = tmp_path / "cwd"
    pkg.mkdir()
    cwd.mkdir()
    # Point "next to module" at a controlled temp dir.
    monkeypatch.setattr(_lib, "__file__", str(pkg / "_lib.py"))
    monkeypatch.chdir(cwd)
    monkeypatch.delenv("LIBDATATRAFFIC_LIB", raising=False)

    name = _lib._lib_name()
    pkg_lib = pkg / name
    cwd_lib = cwd / name
    pkg_lib.write_bytes(b"pkg")
    cwd_lib.write_bytes(b"cwd-decoy")  # must lose to the package dir
    assert _lib._find_lib() == pkg_lib


def test_find_lib_falls_back_to_cwd(tmp_path, monkeypatch):
    from pydatatraffic import _lib

    pkg = tmp_path / "pkg"
    cwd = tmp_path / "cwd"
    pkg.mkdir()
    cwd.mkdir()
    monkeypatch.setattr(_lib, "__file__", str(pkg / "_lib.py"))
    monkeypatch.chdir(cwd)
    monkeypatch.delenv("LIBDATATRAFFIC_LIB", raising=False)

    # No lib next to the module; only present in the working directory.
    cwd_lib = cwd / _lib._lib_name()
    cwd_lib.write_bytes(b"cwd")
    assert _lib._find_lib() == cwd_lib


def test_find_lib_not_found_lists_checked_paths(tmp_path, monkeypatch):
    from pydatatraffic import _lib

    pkg = tmp_path / "pkg"
    cwd = tmp_path / "cwd"
    pkg.mkdir()
    cwd.mkdir()
    monkeypatch.setattr(_lib, "__file__", str(pkg / "_lib.py"))
    monkeypatch.chdir(cwd)
    monkeypatch.delenv("LIBDATATRAFFIC_LIB", raising=False)
    monkeypatch.delenv("LD_LIBRARY_PATH", raising=False)

    with pytest.raises(FileNotFoundError) as ei:
        _lib._find_lib()
    name = _lib._lib_name()
    msg = str(ei.value)
    assert name in msg
    # The error lists the checked candidates, including package and cwd.
    assert str(pkg / name) in msg
    assert str(cwd / name) in msg


# --- _lib_name / arch handling -------------------------------------------------


def test_lib_name_has_current_arch_suffix():
    from pydatatraffic import _lib

    name = _lib._lib_name()
    assert name.startswith("libdatatraffic.so.1.")
    # The suffix must be the normalized current machine arch.
    assert name == "libdatatraffic.so.1." + _lib._target_arch()


def test_target_arch_normalizes_amd64(monkeypatch):
    from pydatatraffic import _lib

    monkeypatch.setattr(_lib.platform, "machine", lambda: "AMD64")
    assert _lib._target_arch() == "x86_64"


def test_get_lib_arch_mismatch_raises(monkeypatch, tmp_path):
    from pydatatraffic import _lib

    # Force the current arch to aarch64 while the found file is x86_64.
    monkeypatch.setattr(_lib.platform, "machine", lambda: "aarch64")
    lib_file = tmp_path / "libdatatraffic.so.1.x86_64"
    lib_file.write_bytes(b"fake")
    monkeypatch.setattr(_lib, "_find_lib", lambda: lib_file)
    monkeypatch.setattr(_lib, "_lib", None)

    with pytest.raises(RuntimeError) as ei:
        _lib._get_lib()
    msg = str(ei.value)
    assert "built for x86_64 but running on aarch64" in msg
    assert "cargo build -p libdatatraffic" in msg
    assert "README.md" in msg
