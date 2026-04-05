# Suggested Commands

## Build & Check
```powershell
cargo check              # Type-check without building
cargo build              # Build the project
cargo build --release    # Release build
cargo run -- <args>      # Run with arguments
cargo clippy             # Lint with clippy
cargo fmt                # Format code
cargo fmt --check        # Check formatting without modifying
```

## Run subcommands
```powershell
cargo run -- web                    # Start web server (port 3000)
cargo run -- web --port 8080        # Start on custom port
cargo run -- download <url|ncode>   # Download a novel
cargo run -- update --all           # Update all novels
cargo run -- convert <target>       # Convert a novel
cargo run -- list                   # List all novels
cargo run -- list --tag <tag>       # Filter by tag
cargo run -- tag --add <tag> <id>   # Add tag
cargo run -- freeze <id>            # Freeze a novel
cargo run -- remove <id>            # Remove a novel
```

## Testing
```powershell
cargo test               # Run all tests
cargo test -- --nocapture # Run tests with stdout
```

## System Commands (Windows)
```powershell
git status
git diff
git log --oneline -20
rg "pattern" --type rust   # Search in Rust files
dir /s /b *.rs             # List all Rust files
```
