# Kilo (Rust)

A very simple text editor in less than 1,000 lines of Rust code.

This is a rewrite of the original Kilo editor (by antirez) and its Go port. This version leverages Rust's safety guarantees to prevent common issues found in other ports.

## Key Improvements
- **No Data Races:** Window resize handling uses thread-safe atomics (`AtomicBool` and `OnceLock`).
- **No Stack Overflows:** Syntax highlighting is iterative, not recursive, ensuring it can handle very large files.
- **Bounds-Safe:** Every slice operation is checked using `>=` and `<=` to prevent "index out of range" panics.
- **Automatic Terminal Restoration:** Uses the RAII pattern (`RawMode` guard) to ensure the terminal is restored even during a panic or crash.
- **Fixed Search:** Correctly maps rendered tab expansion back to the original character index.

## Building
You need the Rust toolchain installed.

```bash
cargo build --release
```

## Usage
```bash
./target/release/kilo <filename>
```

## Controls
- **Arrows:** Move cursor
- **Ctrl-S:** Save
- **Ctrl-F:** Find
- **Ctrl-Q:** Quit
