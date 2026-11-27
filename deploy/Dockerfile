# SPDX-License-Identifier: AGPL-3.0-only
#
# The one Knoggin image for every server role (tab "Deployment", The image). Build it
# from the repository root:
#
#   docker build -f deploy/Dockerfile -t knoggin .
#
# SKIP_MODEL=1 leaves the model out (the stack tests run with the fake model). Release
# builds never set it. CARGO_BUILD_JOBS, when given, caps the Rust build's jobs.
# Every base image is pinned by digest; the tag is in the comment beside it.

ARG SKIP_MODEL=0

# busybox:1.37.0 (the same pin as tests/images.toml). Its static binary gives the
# download stages `unzip` and `xz`, which buildpack-deps:trixie-curl lacks.
FROM busybox@sha256:a46257e06c82ad85242d0d38077d0a52d9896389ca60a1a59d3463db586e5fa4 AS busybox

# ---------------------------------------------------------------------------- ui
# node:26
FROM node@sha256:f8e5d4bb28fa55c74f1345166a082a10b7c68f22304350c1ce0d18358ae94996 AS ui
WORKDIR /src
RUN npm install -g pnpm@12.21.0
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ui/
RUN pnpm -C ui install --frozen-lockfile
COPY ui ui
RUN pnpm -C ui build

# ------------------------------------------------------------------- onnxruntime
# buildpack-deps:trixie-curl
FROM buildpack-deps@sha256:b5d0ed7866dacf9d66c4becf6ffe684c52c39b34be9ac9a60e3efa2e081f942e AS onnxruntime
ARG TARGETARCH
COPY --from=busybox /bin/busybox /usr/local/bin/busybox
RUN ln -s busybox /usr/local/bin/unzip
WORKDIR /src
COPY scripts/fetch-onnxruntime.sh scripts/
COPY third_party/onnxruntime.toml third_party/
RUN set -eu; \
    case "$TARGETARCH" in \
        arm64) target=aarch64-unknown-linux-gnu ;; \
        amd64) target=x86_64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    scripts/fetch-onnxruntime.sh "$target"; \
    mv "target/onnxruntime/$target" /ort; \
    echo "$target" > /ort/target

# ------------------------------------------------------------------------- tools
# The pinned prebuilt cargo-auditable and cargo-about, checked by SHA-256.
FROM buildpack-deps@sha256:b5d0ed7866dacf9d66c4becf6ffe684c52c39b34be9ac9a60e3efa2e081f942e AS tools
ARG TARGETARCH
COPY --from=busybox /bin/busybox /usr/local/bin/busybox
RUN ln -s busybox /usr/local/bin/xz
RUN set -eu; \
    case "$TARGETARCH" in \
        arm64) arch=aarch64; \
               auditable=bcb2d46002a8dd8e94d0d669cb39d38c9651db2ae5d1f3f1f1bab516ee1e212c; \
               about=eb4d9ff6ebd2518152a8308729431a04b3836e71d2bd5ead7aea69f91a71162e ;; \
        amd64) arch=x86_64; \
               auditable=4c8d3b08c3c34e9f2333367224dbbfb4aab68067698b43de59fe19aded3416dd; \
               about=3ad1e94ea1e63d69d1961f4fb15ccbbe9575d906111f96d2cb4dce6df48f71b4 ;; \
        *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --retry 3 -o auditable.tar.xz \
        "https://github.com/rust-secure-code/cargo-auditable/releases/download/v0.7.7/cargo-auditable-$arch-unknown-linux-gnu.tar.xz"; \
    echo "$auditable  auditable.tar.xz" | sha256sum -c -; \
    curl -fsSL --retry 3 -o about.tar.gz \
        "https://github.com/EmbarkStudios/cargo-about/releases/download/0.9.3/cargo-about-0.9.3-$arch-unknown-linux-musl.tar.gz"; \
    echo "$about  about.tar.gz" | sha256sum -c -; \
    mkdir /tools; \
    tar -xJf auditable.tar.xz -C /tools --strip-components=1 --wildcards '*/cargo-auditable'; \
    tar -xzf about.tar.gz -C /tools --strip-components=1 --wildcards '*/cargo-about'

# ------------------------------------------------------------------------- build
# rust:1.100.0-slim-trixie
FROM rust@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS build
ARG CARGO_BUILD_JOBS
COPY --from=tools /tools/ /usr/local/cargo/bin/
WORKDIR /src
# rust-toolchain.toml is left out on purpose: the image already has 1.100.0, and the
# file's extra components would make rustup download them.
COPY Cargo.toml Cargo.lock about.toml about.hbs ./
COPY .cargo .cargo
COPY crates crates
COPY scripts scripts
COPY third_party third_party
COPY --from=ui /src/ui/dist ui/dist
COPY --from=onnxruntime /ort /ort
RUN --mount=type=cache,id=knoggin-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=knoggin-cargo-target,target=/src/target \
    set -eu; \
    if [ -n "${CARGO_BUILD_JOBS:-}" ]; then export CARGO_BUILD_JOBS; else unset CARGO_BUILD_JOBS; fi; \
    cargo auditable build --release --locked -p knoggin; \
    target=$(cat /ort/target); \
    mkdir -p "target/onnxruntime/$target"; \
    cp /ort/LICENSE /ort/ThirdPartyNotices.txt "target/onnxruntime/$target/"; \
    scripts/third-party-notices.sh "$target"; \
    mkdir /out; \
    cp target/release/knoggin THIRD_PARTY_NOTICES /out/

# ------------------------------------------------------------------------- model
# The three model files of tab "Embeddings", from the first source that works, checked
# with sha256sum -c; a mismatch fails the build.
FROM buildpack-deps@sha256:b5d0ed7866dacf9d66c4becf6ffe684c52c39b34be9ac9a60e3efa2e081f942e AS model-0
RUN set -eu; \
    identity=embeddinggemma-2-q4-768-80bc2e03328dae2f; \
    commit=daa72c51243991dfcaf9f9137d2c573d8f7790c0; \
    dir="/out/$identity"; \
    mkdir -p "$dir"; \
    cd "$dir"; \
    for path in onnx/model_q4.onnx onnx/model_q4.onnx_data tokenizer.json; do \
        name="${path##*/}"; \
        curl -fsSL --retry 3 -o "$name" \
            "https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve/$commit/$path" \
        || curl -fsSL --retry 3 -o "$name" \
            "https://github.com/knoggin-memory/knoggin/releases/download/model-$identity/$name"; \
    done; \
    printf '%s\n' \
        "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9  model_q4.onnx" \
        "c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49  model_q4.onnx_data" \
        "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4  tokenizer.json" \
        | sha256sum -c -; \
    for name in model_q4.onnx model_q4.onnx_data tokenizer.json; do : > "$name.ok"; done; \
    chmod 0755 /out "$dir"; \
    chmod 0644 ./*

FROM buildpack-deps@sha256:b5d0ed7866dacf9d66c4becf6ffe684c52c39b34be9ac9a60e3efa2e081f942e AS model-1
RUN mkdir -m 0755 /out

FROM model-${SKIP_MODEL} AS model

# ------------------------------------------------------------------------- final
# gcr.io/distroless/cc-debian13:nonroot
FROM gcr.io/distroless/cc-debian13@sha256:e8343d4a7da8bb210df0c3ea29de38bbadbba6c773bed0cd8bde506fca3a5c44
# The model first, as its own layer, so that it stays byte-identical across releases.
COPY --from=model /out/ /opt/knoggin/models/
COPY --from=onnxruntime /ort/libonnxruntime.so /opt/knoggin/lib/libonnxruntime.so
COPY --from=build /out/THIRD_PARTY_NOTICES /usr/share/doc/knoggin/THIRD_PARTY_NOTICES
COPY --from=build /out/knoggin /usr/local/bin/knoggin
ENV ORT_DYLIB_PATH=/opt/knoggin/lib/libonnxruntime.so \
    KNOGGIN_MODEL__DIR=/opt/knoggin/models \
    KNOGGIN_MODEL__DOWNLOAD=false
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/knoggin"]
