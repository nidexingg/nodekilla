<div align="center">

# 🦀 NODEKILLA.RS

### The simple, fast, and independent `node_modules` grim reaper for devs.
</div>

<div align="center">

**Download the pre-built binaries or executable files from the release assets, or BIY (Build It Yourself):**

Make sure you have [**Rust and Cargo**](https://rust-lang.org/tools/install/) installed on your machine.
</div>

```bash
git clone https://github.com/nidexingg/nodekilla.git
cd nodekilla
cargo build --release
```
<div align="center">

#### Or compile `main.rs` directly with `rustc`:
</div>

```bash
rustc -C opt-level=3 -C lto=fat -C codegen-units=1 -C target-cpu=native src/main.rs
# or simply:
rustc -O src/main.rs
```

<div align="center">
© 2026 Nidexingg Labs - MIT License
</div>
