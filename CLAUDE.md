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

## Architecture

The codebase follows a modular, state-driven architecture:

```
src/
├── main.rs              # GPUI app initialization, window setup, event routing
├── connection/          # Redis connectivity layer
│   ├── config.rs        # Server configuration + TOML persistence
│   ├── async_connection.rs  # Async Redis connection wrapper
│   └── manager.rs       # Connection pooling, topology detection, cluster support
├── states/              # Centralized state management
│   ├── app.rs           # Global app state (theme, locale, font size, routes)
│   ├── server.rs        # Redis server state + event system
│   └── server/          # Data-specific states per Redis type (value, key, string, hash, list, set, zset, stat, event)
├── views/               # GPUI view components (rendering layer)
├── components/          # Reusable UI components
├── helpers/             # Utility functions
└── error.rs             # Error types (snafu-based)
```

### Key Patterns

1. **State Management**: Global store (`ZedisGlobalStore`) for app-wide state; `ZedisServerState` for Redis-specific state. Components subscribe to `ServerEvent` enum for reactive updates.

2. **Async Execution**: Tasks spawned via `cx.spawn(async { ... })`. Long-running Redis commands use GPUI async/await patterns.

3. **Connection Management**: Singleton `ConnectionManager` via `LazyLock`. Auto-detects Standalone/Cluster/Sentinel topologies.

4. **Value Transformation Pipeline**: Raw Redis Value → Format Detection (JSON, MessagePack, Text, Binary) → Decompression (LZ4, SNAPPY, GZIP, ZSTD) → Type-Specific Parsing → Display Formatting

5. **Virtual Scrolling**: Custom `ZedisKvDelegate` for efficient rendering of large key lists with SCAN-based pagination.

## Code Conventions

- **No `.unwrap()`**: Clippy enforces `unwrap_used = "deny"`. Use `?` operator or proper error handling.
- **Error Handling**: All fallible operations return `Result<T, Error>`. Errors logged via `tracing::error!` and converted to user-facing `Notification`.
- **Naming**: `Zedis` prefix for app-specific components. `Event`, `Action`, `State` suffixes for reactive types.
- **View Rendering**: Implement `Render` trait. Use GPUI element builders (v_flex, h_flex, div). Action listeners via `cx.listener()`.

## Configuration

- Server configs stored at `~/.config/zedis/zedis.toml`
- Window bounds, theme, locale persisted via `ZedisAppState` with 500ms debounced saves
