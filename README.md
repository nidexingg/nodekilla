<div align="center">

# 🦀 NODEKILLA.RS

### The simple, fast and independent `node_modules` grim reaper for devs.
</div>

```
git clone https://github.com/nidexingg/nodekilla.git
cd nodekilla
cargo build --release
```
<div align="center">

#### or using `rustc` to compile `main.rs`
</div>

```
rustc -C opt-level=3 -C lto=fat -C codegen-units=1 -C target-cpu=native src/main.rs
# or simply just 
rustc -O src/main.rs
```

<div align="center">
© 2026 Nidexingg Labs - MIT License
</div>