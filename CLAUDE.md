# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Zedis is a high-performance, native Redis GUI client built with Rust and GPUI (the GPU-accelerated rendering engine from Zed Editor). It supports Standalone, Cluster, and Sentinel Redis topologies with automatic data format detection (JSON, MessagePack, compressed data).

## Common Commands

```bash
make lint          # cargo clippy --all-targets --all -- --deny=warnings
make fmt           # cargo fmt
make dev           # bacon run (hot reload during development)
make debug         # RUST_LOG=DEBUG make dev (with debug logging)
make release       # cargo build --release --features mimalloc
make bundle        # cargo bundle --release --features mimalloc (create native installers)
make udeps         # cargo +nightly udeps (find unused dependencies)
```

## Toolchain

- Rust 1.92.0+
- rustfmt: `max_width = 120`
- Clippy: `unwrap_used = "deny"` (在 Cargo.toml 中配置)

## Architecture

State-driven architecture: `connection/` (Redis 连接层) → `states/` (状态管理) → `views/` (GPUI 视图) → `components/` (可复用组件)。`helpers/` 包含编解码等工具函数。

### Key Patterns

- **State**: `ZedisGlobalStore` (全局) + `ZedisServerState` (每个 Redis 连接)，组件通过 `ServerEvent` 响应式更新
- **Async**: 通过 `cx.spawn(async { ... })` 执行异步任务
- **Connection**: `ConnectionManager` (LazyLock 单例)，自动检测 Standalone/Cluster/Sentinel
- **Value Pipeline**: Raw Value → Format Detection (JSON/MsgPack/Text/Binary) → Decompression (LZ4/SNAPPY/GZIP/ZSTD) → Display

## Code Conventions

- **No `.unwrap()`**: 使用 `?` 或 proper error handling
- **Error Handling**: `Result<T, Error>` + `tracing::error!` 记录日志，转为 `Notification` 展示给用户
- **Naming**: `Zedis` 前缀用于 app 组件，`Event`/`Action`/`State` 后缀用于响应式类型
- **View Rendering**: 实现 `Render` trait，使用 GPUI element builders (v_flex, h_flex, div)，`cx.listener()` 绑定事件

## Configuration

- Server configs stored at `~/.config/zedis/zedis.toml`
- Window bounds, theme, locale persisted via `ZedisAppState` with 500ms debounced saves

## 仓库
默认仓库为fork仓库
