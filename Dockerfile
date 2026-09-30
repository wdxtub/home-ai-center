# ── 第一阶段：构建前端 ─────────────────────────────────────────────
# 产物落在 web/dist，Rust 侧用 include_dir! 在**编译期**内嵌进二进制。
# 因此顺序不能反：必须先有 dist，cargo 才能编过。
FROM node:22-alpine AS web

WORKDIR /build
COPY web/package.json web/package-lock.json* ./
# 依赖层单独缓存：改前端代码不会触发重装
RUN npm ci --no-audit --no-fund

COPY web/ ./
RUN npm run build && mv dist /tmp/webdist


# ── 第二阶段：构建后端 ─────────────────────────────────────────────
FROM rust:1-alpine AS api

# sqlx 的 sqlite 特性需要 cc 与 C 工具链
RUN apk add --no-cache musl-dev make gcc perl

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
# 迁移 SQL 必须编进二进制，容器里不再单独拷
COPY --from=web /tmp/webdist /build/web/dist

RUN cargo build --release --locked -p home-ai-center
RUN strip target/release/home-ai-center || true


# ── 第三阶段：运行时 ───────────────────────────────────────────────
FROM alpine:3.20

# sqlite3 便于手工查库；tzdata 供时区计算用（禁用时段、每日分桶）
RUN apk add --no-cache ca-certificates tzdata sqlite \
    && addgroup -g 10001 -S hac \
    && adduser -u 10001 -S hac -G hac

COPY --from=api /build/target/release/home-ai-center /usr/local/bin/home-ai-center

# 数据目录：SQLite、备份、图片归档都在这里，挂出去即可持久化
RUN mkdir -p /data && chown hac:hac /data
VOLUME ["/data"]

USER hac
WORKDIR /data

ENV HOME_AI_DATA_DIR=/data \
    HOME_AI_BIND=0.0.0.0:8080 \
    HOME_AI_TIMEZONE=Asia/Shanghai \
    HOME_AI_BACKUP_AT=03:30 \
    RUST_BACKTRACE=1

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD sqlite3 /data/home-ai-center.db "SELECT 1" >/dev/null 2>&1 || exit 1

ENTRYPOINT ["/usr/local/bin/home-ai-center"]
