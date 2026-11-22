# Shopping Sessions: native Rust pipeline.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --release && rm -rf src
COPY src src
RUN touch src/main.rs && cargo build --release && strip target/x86_64-unknown-linux-gnu/release/sessions

FROM debian:bookworm-slim
COPY --from=build /src/target/x86_64-unknown-linux-gnu/release/sessions /usr/local/bin/sessions
# mimalloc: back large allocations with transparent huge pages, never hand memory back mid-run
ENV MIMALLOC_ALLOW_LARGE_OS_PAGES=1 MIMALLOC_PURGE_DELAY=-1
USER 1000:1000
CMD ["/usr/local/bin/sessions"]
