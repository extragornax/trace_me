FROM rust:latest AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

FROM debian:12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates wget && rm -rf /var/lib/apt/lists/*
RUN useradd -m app
USER app
WORKDIR /app
COPY --from=builder /build/target/release/trace_gpx .
COPY static/ static/
EXPOSE 3000
CMD ["./trace_gpx"]
