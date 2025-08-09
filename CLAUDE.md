# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Common Development Commands

**⚠️ Build Performance Note**: This project takes a while to build. The restate-server binary especially can take several minutes - be patient. Favor the dev profile for debug builds to optimize compile times.

### Build Commands
- `just build` - Build all packages with default target
- `just build --bin restate-server` - Build the main server binary (slow - be patient)
- `just build --bin restate` - Build the CLI binary
- `just build --release` - Build optimized release binaries
- `cargo build` - Alternative build via cargo directly

**Direct Binary Builds:**
- `cargo build --bin restate-server` - Build server binary directly (slow - be patient)
- `cargo build --bin restate` - Build CLI binary directly
- `cargo build --bin restatectl` - Build operator-focused CLI tool directly

### Testing
- `just test` - Run all tests using nextest (recommended)
- `cargo nextest run --workspace` - Alternative nextest command
- `just test-package <package>` - Run tests for specific package
- `just doctest` - Run documentation tests

### Code Quality
- `just fmt` - Format code
- `just check-fmt` - Check code formatting
- `just clippy` - Run clippy linting
- `just lint` - Run all lints (fmt, clippy, deny)
- `just verify` - Run lints and tests together (takes ~5-10 minutes, be patient)
- **🚨 Always run `just verify` at the end** - This runs format/lint checks and tests. Takes several minutes to complete - don't timeout, wait for results.

### Other Useful Commands
- `just clean` - Clean build artifacts
- `just check-deny` - Check dependency licensing
- `just flamegraph --bin restate-server` - Performance profiling
- `just docker` - Build Docker image

## High-Level Architecture

Restate is a distributed durable execution platform built in Rust with a modular architecture:

### Core Components

**Binary Targets:**
- `restate-server` (server/) - Main runtime server with pluggable roles (admin, ingress, worker)
- `restate` (cli/) - Command-line interface for cluster management

**Key Crates:**
- `restate-core` - Task center, metadata management, networking foundation
- `restate-node` - Node management and role coordination
- `restate-types` - Common types, configuration, and schemas
- `restate-bifrost` - Distributed log system with pluggable storage
- `restate-worker` - Partition processing and state management
- `restate-admin` - Cluster administration and REST API
- `restate-ingress-*` - HTTP and Kafka ingress handling
- `restate-partition-store` - RocksDB-based partition storage
- `restate-metadata-*` - Distributed metadata management

### Service Architecture

Restate follows a role-based architecture where nodes can run different combinations of:
- **Admin Role**: Cluster management, REST API, web UI
- **Worker Role**: Partition processing, state management, service invocation
- **Ingress Role**: Request handling (HTTP/gRPC), protocol adaptation

The system uses:
- **Bifrost** for distributed logging and replication
- **RocksDB** for persistent state storage
- **gRPC** for inter-node communication
- **Task Center** for async task management and graceful shutdown

### Development Structure

- Configuration via TOML files or environment variables
- Uses `just` for task automation (see justfile)
- Protocol Buffers for service definitions
- Extensive use of derive macros for code generation
- Test utilities in `test-util` crate for integration tests

## Configuration

The system uses configuration profiles with production defaults available via `--production` flag. Config files are typically in TOML format and can be dumped with `--dump-config`.

## Code Style

General:

- don't write useless tests; asserting simple properties that are evident by looking at the code adds no value
- minimize comments - avoid stating the obvious; only add comments to explain _why_ something non-obvious is done
- no doc comments on obvious functions or private implementations; reserve for public APIs that need explanation
- when in doubt, prefer clear code over comments

## Performance-minded Defaults

- favor `ahash::HashMap` over `std::collections::HashMap`

### Import Grouping
Imports should be grouped in the following order, separated by empty lines:

1. **std imports** - Standard library imports
2. **Third-party crates** - External dependencies
3. **Restate crates** - Internal restate-* crates
4. **Crate-local imports** - Local `crate::` and `super::` imports

Example:
```rust
use std::io;
use std::time::Instant;

use anyhow::Result;
use clap_repl::ClapEditor;
use cling::prelude::*;
use serde::{Deserialize, Serialize};

use restate_cli_util::c_println;
use restate_types::SomeType;

use crate::cli_env::CliEnv;
```

### Cargo.toml Dependencies
Dependencies in Cargo.toml should be sorted alphabetically within groups:

1. **Restate crates** - Internal restate-* dependencies (sorted alphabetically)
2. **Third-party crates** - External dependencies (sorted alphabetically)

Keep restate crates in a separate group from third-party dependencies for clarity.

## Documentation
- Each major component should have its own CLAUDE.md for component-specific notes
- Keep this root file focused on project-wide conventions
