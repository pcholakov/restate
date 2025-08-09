# CLI Development Notes for Claude

Component-specific notes for working on the Restate CLI.

## Architecture

The CLI is built using:
- **cling** - CLI framework with derive macros
- **clap** - Argument parsing (used via cling)
- **figment** - Configuration management 
- **clap-repl** - REPL functionality

## Key Components

### Commands Structure
- All commands are in `src/commands/`
- Each command module follows the pattern: `Command` struct + `run_*` function
- Commands use `#[derive(Run, Parser, Collect, Clone)]` and `#[cling(run = "run_function")]`

### REPL Implementation Notes
When adding REPL functionality:

1. **Avoid Code Duplication**
   - Extract core logic into separate functions
   - Reuse existing command structures rather than recreating them
   - Don't duplicate output formatting logic

2. **Async Handling in REPL**
   - Use proper async context instead of `futures::executor::block_on()`
   - Consider restructuring to handle async at higher levels
   - Avoid nested async runtime creation

3. **Exit Handling**
   - Use `return` from REPL loop instead of `std::process::exit()`
   - Allow proper cleanup and shutdown

4. **State Management**
   - Minimize stateful variables in REPL loops
   - Reuse existing flag/option patterns where possible

## Environment & Configuration

- `CliEnv` manages environment configuration
- Supports multiple environments via `--environment` flag
- Configuration sources: CLI args → env vars → config files → defaults

## Common Patterns

### Command Implementation
```rust
#[derive(Run, Parser, Collect, Clone)]
#[cling(run = "run_command")]
pub struct MyCommand {
    // fields
}

pub async fn run_command(State(env): State<CliEnv>, opts: &MyCommand) -> Result<()> {
    // implementation
}
```

### Adding New Commands
1. Create module in `src/commands/`
2. Add to `src/commands/mod.rs`
3. Add variant to `Command` enum in `src/app.rs`

## Testing
- Integration tests should use the existing CLI test utilities
- Mock external dependencies through the environment configuration