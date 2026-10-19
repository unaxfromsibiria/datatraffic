import os
import platform
import re
import shutil
import subprocess
from pathlib import Path

from setuptools import setup
from setuptools.command.build_py import build_py as _build_py


class BuildPy(_build_py):
    """Keep only real modules in the wheel (drop test_*.py)."""

    def find_package_modules(self, package, package_dir):
        return [
            m for m in super().find_package_modules(package, package_dir)
            if not m[1].startswith("test_")
        ]


NAME = "pydatatraffic"
HERE = Path(__file__).resolve().parent
PKG_DIR = HERE / NAME
_ARCH_ALIASES = {
    "amd64": "x86_64",
    "arm64": "aarch64",
}


def _target_arch():
    """Current machine arch normalized to the cargo target-arch name."""
    machine = platform.machine().lower()
    return _ARCH_ALIASES.get(machine, machine)


def _ensure_lib():
    """Build the cdylib with cargo and copy it into the package.

    Uses a release build: this runs during packaging, so the shipped library
    is optimized (dev iteration uses `make build-lib`, which stays on debug).
    Runs before any build command so package_data picks up the .so. If cargo
    is missing but a prebuilt library for this arch exists, reuse it.
    """
    arch = _target_arch()
    lib_name = "libdatatraffic.so.1." + arch
    target_so = PKG_DIR / lib_name
    if shutil.which("cargo"):
        env = dict(os.environ)
        # SONAME via RUSTFLAGS (same as `make build-lib`).
        env["RUSTFLAGS"] = "-C link-args=-Wl,-soname," + lib_name
        subprocess.check_call(
            ["cargo", "build", "--release", "-p", "libdatatraffic"],
            cwd=HERE,
            env=env,
        )
        shutil.copyfile(
            HERE / "target" / "release" / "libdatatraffic.so", target_so
        )
        return
    if target_so.exists():
        print("cargo not found; using prebuilt %s" % lib_name)
        return
    raise RuntimeError(
        "%s not found and cargo is unavailable; install the Rust toolchain "
        "(see README.md) or run 'make build-lib' first" % lib_name
    )


def version() -> str:
    """Extract the package version from pydatatraffic/__init__.py."""
    for line in (PKG_DIR / "__init__.py").read_text().splitlines():
        if "__version__" in line:
            *_, val = line.rsplit("=", 1)
            return ".".join(re.findall(r"\d+", val))

    raise RuntimeError(f"No version number in {NAME}")


def read_requirements(filename: str) -> list[str]:
    """Read dependency lines from a requirements file inside the package."""
    reqs = []
    for line in (PKG_DIR / filename).read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            reqs.append(line)
    return reqs


_ensure_lib()

README = HERE / "README.md"

setup(
    name=NAME,
    version=version(),
    author="Michael Vorotyntsev",
    author_email="linkofwise@gmail.com",
    url="https://github.com/unaxfromsibiria/datatraffic",
    description=(
        "Python bindings for libdatatraffic: encrypted byte streams over Redis."
    ),
    long_description=README.read_text() if README.exists() else "",
    long_description_content_type="text/markdown",
    packages=[NAME],
    package_dir={NAME: NAME},
    package_data={NAME: ["libdatatraffic.so.1.*", "requirements.txt"]},
    include_package_data=True,
    cmdclass={"build_py": BuildPy},
    install_requires=read_requirements("requirements.txt"),
    python_requires=">=3.10",
    platforms="any",
    zip_safe=False,
)
