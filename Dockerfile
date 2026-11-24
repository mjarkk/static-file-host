FROM rust:1.100-slim-trixie AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends tini \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/static-file-host /usr/local/bin/
RUN mkdir /data

FROM gcr.io/distroless/cc-debian13:nonroot
# As PID 1 the server would ignore SIGTERM; tini forwards it.
COPY --from=build /usr/bin/tini-static /usr/bin/tini
COPY --from=build /usr/local/bin/static-file-host /usr/local/bin/
# A fresh named volume copies this owner, so nonroot can write to it.
COPY --from=build --chown=nonroot:nonroot /data /data
ENV MALLOC_ARENA_MAX=2
# --data defaults to ./data, so `serve` and `token` both find /data.
WORKDIR /
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/static-file-host"]
CMD ["serve", "--listen", "0.0.0.0:8080"]
