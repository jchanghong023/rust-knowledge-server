# Repository Guidelines

## Project Overview

Rust Knowledge Server is a standalone service for importing shared UTF-8 Markdown and searching published libraries from a browser or read-only MCP. It returns original excerpts with relative paths, one-based line ranges, and source SHA-256 hashes.

## Architecture & Data Flow

- **Startup:** `src/main.rs` constructs the catalog, audit store, and optional model clients, then mounts the Web API and native Streamable HTTP `/mcp` on one Axum listener. Qdrant Edge runs in process.
- **Import:** `src/web.rs` starts an audited operation; `src/import.rs` streams and validates multipart uploads into temporary storage. `src/markdown.rs` preserves source coordinates and `src/index.rs` builds a fresh Edge generation. `Catalog::publish` seals and atomically commits it after build handles close. Reimport replaces the entire library; rejected or failed builds must not publish partial data.
- **Search:** Web and MCP call `search::search` in `src/search.rs`. It pins the selected published generations before opening shards or awaiting models, then runs scoped BM25 and exact technical-name retrieval, optionally fusing compatible embeddings and reranking a bounded window. Keep retrieval changes shared. Model failure falls back to keyword or base ranking; missing or corrupt shard data is an error, not zero hits.

## Key Directories

- `src/` — binary, storage and retrieval modules, protocol adapters, and Rust tests.
- `web/` — embedded Simplified Chinese HTML, CSS, and JavaScript. Rebuild the binary to pick up changes; don't open the HTML as a standalone page.
- `release/` — CentOS 7 build script and the version-specific Edge patch.
- `openspec/changes/add-standalone-knowledge-server/` — behavioral specs, design decisions, and verification tasks.

## Development Commands

| Task | Command |
| --- | --- |
| Build | `cargo build --locked` |
| Format check | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --locked --all-targets` |
| Test | `cargo test --locked` |
| CLI help | `cargo run --locked -- --help` |
| Serve locally | `cargo run --locked -- --listen 127.0.0.1:9333` |
| Windows x64 release | `cargo build --locked --release --target x86_64-pc-windows-msvc` |
| CentOS 7 x64 release | `bash release/build-centos7.sh` on an x86_64 glibc 2.17 host |

Open `http://127.0.0.1:9333` for the UI. On Linux, the binary also supports `start [startup options]`, `status`, and `stop`.

## Code Conventions & Common Patterns

- Follow rustfmt and Rust naming (`snake_case` modules and functions, `PascalCase` types). Default to private items; use `pub(crate)` for interfaces shared across modules. Return typed `Result` errors; map them at the transport seam (`ApiError` in Web, MCP errors in `src/mcp.rs`). Keep query text, document bodies, and credentials out of diagnostics.
- Inject dependencies through router constructors. Web handlers extract `State<Arc<Web>>`; MCP sessions clone a `KnowledgeServer` backed by the same catalog. Keep synchronous filesystem and Edge work on bounded blocking workers, model HTTP async, and service-owned jobs in `TaskTracker` so shutdown drains them.
- Keep Web's self-reported employee session separate from authentication and from MCP. Preserve the audit ordering: record the start before work; for mutations, commit the catalog receipt with the change and clear it only after the audit outcome is durable. An uncertain commit is not a normal failure to retry.

## Important Files

- `src/main.rs` and `src/settings.rs` — startup, shared HTTP Host checks, CLI parsing and help. `src/daemon.rs` owns Linux background control.
- `src/catalog.rs`, `src/durable.rs`, and `src/shard.rs` — publication, integrity checks, and lazy open of pinned generations. `src/data_root.rs` owns platform checks and the exclusive lock.
- `src/model_http.rs` and `src/model_defaults.rs` — optional model clients and Linux-only gateway defaults. `Cargo.toml`, `Cargo.lock`, and `.cargo/config.toml` define dependencies and Windows static CRT flags.
- `PRODUCT.md` and `DESIGN.md` describe Web behavior and design. `README.md` is the user-facing quickstart. Do not modify `README.md` unless the user explicitly requests a README change.

## Runtime/Tooling Preferences

- Use Rust/Cargo (edition 2024) with the checked-in lockfile. No Node, Bun, frontend package manager, separate database process, or runtime asset download. Keep Edge at the pinned version unless deliberately updating its CentOS patch and compatibility checks.
- `release/build-centos7.sh` uses Cargo, `patch`, and Python 3 to vendor and patch dependencies in scratch space before an offline build. A plain Linux release build does not establish glibc 2.17 compatibility.
- No service config file or data-root override. The root is `~/.rust-knowledge-server` on Windows and `/dev/shm/rust-knowledge-server` on Linux (lost on machine reboot). Default bind is `0.0.0.0:9333`; use loopback for local work. Models are optional and independently configured; on Linux, `OMP_OFFLINE=1` can fill missing model settings from the existing Claude Code gateway config. Use `--help` for supported flags.

## Testing & QA

- Use Cargo's built-in harness (`#[test]`, `#[tokio::test]`). Most tests are inline or in `src/<module>/tests.rs`; `src/surface_tests.rs` runs real Web and MCP routers over loopback. Focus with `cargo test --locked search::tests::` or `cargo test --locked surface_tests::`.
- On Windows, `cargo test --locked --test search_quality -- --nocapture` launches the service binary with temporary `USERPROFILE` directories and checks synthetic DFT, code Wiki, and merged Markdown queries over HTTP in keyword, hybrid, and reranked modes. Model endpoints are deterministic loopback stubs; the test does not read real documents or evaluate a real model.
- Use small, deterministic Markdown fixtures, temporary data roots, and real Edge shards. Stub only model HTTP endpoints on loopback; don't require real credentials. Assert original evidence, required relative ordering, and relevant failure states rather than pinning entire rankings. No checked-in coverage threshold.
- Linux storage fixtures require private directories on tmpfs; use `(umask 077; TMPDIR=/dev/shm cargo test --locked)` where `/dev/shm` is writable. On Windows use a local fixed-volume temp directory.
- For UI changes, check the running server in a real browser. Target-host deployment acceptance is separate: follow §4 of `openspec/changes/add-standalone-knowledge-server/tasks.md`; local tests and mocks don't replace it.
