# RUST IMPLEMENTATION

**Generated:** 2026-01-15
**Commit:** c19788c
**Branch:** main

## OVERVIEW

Rust implementation of LocalSend protocol (template stage). Provides CLI client and HTTP server using Axum framework. Protocol v2.1 compatible with feature-gated modules.

## STRUCTURE

```
./
├── src/
│   ├── cli/           # Command-line interface (clap-based)
│   ├── client/        # HTTP client for sending files
│   ├── discovery/     # UDP multicast discovery
│   ├── protocol/      # Protocol types and constants
│   └── server/       # HTTP server (Axum-based)
├── Cargo.toml        # Feature-gated dependencies
└── downloads/        # Default download directory
```

## WHERE TO LOOK

| Task                | Location                    | Notes                              |
| ------------------- | --------------------------- | ---------------------------------- |
| CLI entry           | `src/main.rs`               | send/receive/discover commands      |
| CLI commands        | `src/cli/`                  | clap command definitions            |
| Discovery           | `src/discovery/multicast.rs`   | UDP multicast (224.0.0.167:53317)|
| HTTP server         | `src/server/server.rs`        | Axum server (info, register)      |
| Protocol types      | `src/protocol/types.rs`       | LocalSend v2.1 DTOs             |
| Cryptography       | `src/crypto.rs`              | SHA-256, fingerprints            |

## CONVENTIONS

- **Feature gates**: Optional `cli` and `https` features in `Cargo.toml`
- **Error handling**: `anyhow` for applications, `thiserror` for library components
- **Async runtime**: Tokio for all async operations (discovery, server, client)
- **Protocol constants**: Mirrors TypeScript implementation (multicast IP, port, API version)
- **Module structure**: Clean domain separation (discovery, protocol, server, client)

## ANTI-PATTERNS

- **DO NOT assume complete implementation**: Currently a template - upload logic not implemented
- **DO NOT use without `cli` feature**: Binary only builds with `cli` feature enabled
- **DO NOT hardcode ports**: Use constants from `src/protocol/constants.rs`
- **NEVER skip fingerprint checks**: SHA-256 fingerprint prevents self-discovery

## DEVELOPMENT STATUS

- **Discovery**: Functional (multicast UDP)
- **HTTP server**: Partial (info/register routes only)
- **File upload**: Not implemented
- **File download**: Not implemented
- **CLI**: Basic structure exists, commands need completion

## REFERENCE

- **Complete Rust impl**: `reference/localsend-rs-1/` and `reference/localsend-rs-2/`
- **Protocol spec**: `reference/protocol/README.md`
- **TypeScript reference**: `../localsend-ts/src/` (mature implementation)
