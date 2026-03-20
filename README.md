[中文](./README_zh.md) | English

# Zedis (Fork)

A High-Performance, GPU-Accelerated Redis Client Built with **Rust** 🦀 and **GPUI** ⚡️

> This project is forked from [vicanso/zedis](https://github.com/vicanso/zedis).
>
> I really love the UI design of this client — it's built on GPUI (the rendering engine behind Zed Editor), making it incredibly fast and visually appealing. However, the feature development pace of the original project is a bit slow for my needs, and submitting PRs requires the original author's review which feels cumbersome. So I decided to fork it and maintain my own version with the features I need.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

![Zedis](./assets/zedis.png)

---

## 🔀 Fork Changes (since v0.3.0)

Features added in this fork that are not available in the original repository:

### Key Management
- **Key Rename**: Inline key rename support using Redis `RENAME` command
- **Multi-select & Batch Delete**: Select multiple keys and delete them in batch
- **Key Export/Import**: Export and import keys via `DUMP`/`RESTORE` with ARDM-compatible CSV format
- **Right-click Context Menu**: Duplicate and delete keys directly from the key tree context menu
- **Refresh Keys**: Refresh key list while preserving the current keyword filter

### Editor Enhancements
- **Advanced Edit Dialog**: Format conversion (JSON, MessagePack, Text, Binary) and compression (LZ4, SNAPPY, GZIP, ZSTD) support in the edit dialog
- **Selectable Text**: Key names and dialog titles are selectable and copyable
- **Search Shortcut**: Focus-aware `Cmd+F` / `Ctrl+F` search within the editor
- **Protobuf Support**: Raw protobuf format detection and schema-based decoding

### Connection & Sidebar
- **Duplicate Connections**: Quickly duplicate existing server connections
- **Connection List Layout Toggle**: Switch between different saved-connection list layouts
- **Edit Connection**: Edit connection settings directly from the sidebar right-click menu
- **Sidebar Tooltips**: Show full server name on hover
- **Right-click Close**: Close server connections via right-click context menu
- **Keyword Preservation**: Preserve search keyword when switching between servers

### Search & Navigation
- **Home Search Box**: Global search with `Cmd+F` / `Ctrl+F` shortcut on the home page
- **Add Server Button**: Moved to search bar area with tooltip for better accessibility

### Auth & Credentials
- **Preset Credentials**: Auto-authentication with preset credentials support

### Developer Experience
- **File Logging**: File-based logging support for debugging

---

## 📖 Introduction

**Zedis** is a next-generation Redis GUI client designed for developers who demand speed.

Unlike Electron-based clients that can feel sluggish with large datasets, Zedis is built on **GPUI** (the same rendering engine powering the [Zed Editor](https://zed.dev)). This ensures a native, 60 FPS experience with minimal memory footprint, even when browsing millions of keys.

## 📦 Installation

Download the latest pre-built binaries from the [GitHub Releases](https://github.com/cyenxchen/zedis/releases) page.

Available platforms: **macOS** (aarch64 / x86_64), **Windows**, **Linux**.

## ✨ Features

### 🚀 Blazing Fast
- **GPU Rendering**: All UI elements are rendered on the GPU for buttery smooth performance.
- **Virtual List**: Efficiently handle lists with 100k+ keys using virtual scrolling and `SCAN` iteration.

### 🧠 Smart Data Viewer
Zedis automatically detects content types (`ViewerMode::Auto`) and renders them in the most useful format:
- **Automatic Decompression**: Transparently detects and decompresses **LZ4**, **SNAPPY**, **GZIP** and **ZSTD** data, allowing you to view the actual content (e.g., compressed JSON will be automatically unpacked and pretty-printed).
- **JSON**: Automatic **pretty-printing** with full **syntax highlighting** for better readability.
- **MessagePack**: deserializes binary MsgPack data into a readable JSON-like format.
- **Images**: Native preview for stored images (`PNG`, `JPG`, `WEBP`, `SVG`, `GIF`).
- **Hex View**: Adaptive 8/16-byte hex dump for analyzing raw binary data.
- **Text**: UTF-8 validation with large text support.

### 🛡️ Secure Access
- **SSH Tunneling**: Securely access private Redis instances via bastion hosts. Supports authentication via Password, Private Key, and SSH Agent.
- **TLS/SSL**: Full support for SSL/TLS encrypted connections, including options for custom CA, Client Certificates, and Private Keys.

### 🎨 Modern Experience
- **Cross-Platform**: Powered by GPUI, Zedis delivers a consistent, high-performance native experience across **macOS**, **Windows**, and **Linux**.
- **Smart Topology Detection**: Automatically identifies and adapts to **Standalone**, **Cluster**, or **Sentinel** modes. Just connect to an entry node, and Zedis handles the topology mapping without complex configuration.
- **Themes**: Pre-loaded with **Light**, **Dark**, and **System** themes.
- **I18n**: Full support for **English** and **Chinese (Simplified)**.
- **Responsive**: Split-pane layout that adapts to any window size.

## 📄 License

This project is Licensed under [Apache License, Version 2.0](./LICENSE).