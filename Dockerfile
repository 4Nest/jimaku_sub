# 构建阶段
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 先复制依赖文件，利用 Docker 缓存层
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release && rm -rf src

# 复制源码并构建
COPY . .
RUN touch src/main.rs
RUN cargo build --release

# 运行阶段
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/jimaku-subscriber /usr/local/bin/jimaku-subscriber

# 创建数据目录、下载目录和日志目录
RUN mkdir -p /app/data /app/downloads /app/logs

ENV RUST_LOG=info
ENV DATABASE_URL=sqlite:///app/data/jimaku_subscriber.db

VOLUME ["/app/data", "/app/downloads", "/app/logs"]

ENTRYPOINT ["jimaku-subscriber"]
