FROM rust:1.92.0-bookworm@sha256:9676d0547a259997add8f5924eb6b959c589ed39055338e23b99aba7958d6d31 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY index.html ./
RUN cargo build --release

FROM debian:bookworm-slim@sha256:78d2f66e0fec9e5a39fb2c72ea5e052b548df75602b5215ed01a17171529f706
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/compose-manager-dashboard /usr/local/bin/
CMD ["compose-manager-dashboard"]
