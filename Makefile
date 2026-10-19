# source ./venv/bin/activate

.PHONY: build-lib lint isort fisort test complextest package dockerbuild

# Image name for `dockerbuild` (python package builder).
DOCKER_IMAGE := pydatatraffic-builder

ARCH := $(shell rustc -vV 2>/dev/null | awk '/^host:/ {print $$2}' | cut -d- -f1)

build-lib:
	RUSTFLAGS="-C link-args=-Wl,-soname,libdatatraffic.so.1.$(ARCH)" cargo build -p libdatatraffic
	rm -f pydatatraffic/libdatatraffic.so pydatatraffic/libdatatraffic.so.1*
	cp target/debug/libdatatraffic.so pydatatraffic/libdatatraffic.so.1.$(ARCH)

lint:
	flake8 ./pydatatraffic

isort:
	isort -c --diff ./pydatatraffic

fisort:
	isort ./pydatatraffic

test: build-lib
	python -m pytest -vv ./pydatatraffic
	cargo test

# End-to-end pub/sub pipeline test (3 clients, 3 projects) on rust/python/mixed stacks.
# Requires REDIS_URL and CRYPTO_KEY in the environment (REDIS_PASSWORD optional).
complextest: build-lib
	bash scripts/complextest.sh --stack rust
	bash scripts/complextest.sh --stack python
	bash scripts/complextest.sh --stack mixed

package:
	rm -rf dist
	python -m build --wheel --sdist --outdir dist/ .

# Build the python package in docker and move artifacts out of the image into
# /tmp/datatraffic. Cleans the destination first, lists its contents at the end.
dockerbuild:
	rm -rf /tmp/datatraffic && mkdir -p /tmp/datatraffic
	docker build -t $(DOCKER_IMAGE) .
	@cid=$$(docker create $(DOCKER_IMAGE)); \
	docker cp $$cid:/app/dist/. /tmp/datatraffic/; \
	rc=$$?; docker rm $$cid >/dev/null; exit $$rc
	ls -l /tmp/datatraffic
