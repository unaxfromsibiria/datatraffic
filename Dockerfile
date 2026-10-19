FROM python:3.13-trixie

RUN apt-get update
RUN apt-get install -y --no-install-recommends build-essential curl ca-certificates pkg-config
RUN rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && . "$HOME/.cargo/env" && rustc --version

ENV PATH="/root/.cargo/bin:${PATH}"

RUN pip install --no-cache-dir build

WORKDIR /app
COPY . .

RUN python -m build --wheel --sdist --outdir dist/ .
