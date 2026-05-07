---
name: explore-redis
description: Guided exploration of Zedis architecture and core modules
---

Help the user understand the Zedis codebase by exploring these key areas based on $ARGUMENTS:

## Core modules to explore

- **Connection layer** (`src/connection/`): `manager.rs` handles topology detection (Standalone/Cluster/Sentinel), `config.rs` handles TOML persistence, `async_connection.rs` wraps async Redis ops, `ssh_tunnel.rs` for SSH tunneling
- **State management** (`src/states/`): `app.rs` for global store (theme/locale/routes), `server.rs` for per-connection state + ServerEvent system, `server/` subdirectory for type-specific states (string, hash, list, set, zset, value, key)
- **Value pipeline** (`src/helpers/codec.rs`): Format detection (JSON/MsgPack/Protobuf/Hex/Image), decompression (LZ4/SNAPPY/GZIP/ZSTD), the full transformation chain
- **Views** (`src/views/`): GPUI rendering layer, `key_tree.rs` for virtual scrolling key list, `editor.rs` for main editor
- **Entry point** (`src/main.rs`): GPUI app init, window setup, event routing

If no specific area is requested, provide a high-level overview of the architecture and ask what the user wants to dive deeper into.
