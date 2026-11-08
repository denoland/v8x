`v8x` makes rusty_v8 engine agnostic.

Docs: https://littledivy.github.io/v8x · Test dashboard: https://littledivy.github.io/v8x/status/

```diff
-v8 = "0.155.0"
+v8 = { package = "v8x", version = "0.155.0", features = ["jsc"] }
```

Supported engines:

- V8 15.2.124.1-rusty
- JavaScriptCore / WebKit 625.1+ and System-framework path uses the OS's JSC.
- QuickJS-ng 0.15.1 


Swap the engine under Deno without touching `deno_core`:

```toml
# deno's workspace Cargo.toml
v8 = { package = "v8x", version = "0.155.0", features = ["jsc"] }
```

```diff
- cargo build -p deno
+ cargo build -p deno --features hmr
```

`v8x` vendors the real `v8` crate's Rust source and implements the `v8__*` C ABI
on the chosen engine, so the swap is a drop-in — `deno_core` compiles unchanged.

| engine | deno size | engine size |
| --- | --- | --- |
| Deno V8 15.2 | 78.7 MB | ~40 MB static |
| Deno JSC | 80.7 MB | ~48 MB static |
| Deno system JSC | 54.2 MB | 0 |
| Deno quickjs-ng | 56.1 MB | ~1 MB static |
