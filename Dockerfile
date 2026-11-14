# 构建阶段（纯 Rust TLS + 内置 SQLite，无需 OpenSSL 开发库）
FROM rust:slim-bookworm AS builder

WORKDIR /app

# 先复制依赖清单预编译缓存层
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# 复制源码并构建
COPY . .
RUN touch src/main.rs && cargo build --release

# 运行阶段
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /app/target/release/jimaku-subscriber /usr/local/bin/jimaku-subscriber

# 创建数据目录、下载目录和日志目录
RUN mkdir -p /app/data /app/downloads /app/logs

ENV RUST_LOG=info
ENV DATABASE_URL=sqlite:///app/data/jimaku_subscriber.db

VOLUME ["/app/data", "/app/downloads", "/app/logs"]

ENTRYPOINT ["jimaku-subscriber"]
