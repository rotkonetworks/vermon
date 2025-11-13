FROM rust:1.82 AS builder
WORKDIR /app

# Copy dependency manifests and build dependencies first (cached layer)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release && rm -rf src target/release/deps/vermon*

# Now copy actual source and build (only rebuilds if source changed)
COPY src ./src
COPY templates ./templates
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/vermon /usr/local/bin/vermon
COPY static /app/static
COPY templates /app/templates
EXPOSE 3000
CMD ["vermon"]
