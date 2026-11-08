#import "./shim/html.typ": *

#set document(
  title: "getting started · v8x",
  description: "Swap the JavaScript engine under rusty_v8 or Deno with a one-line Cargo change.",
)

#show: html-shim

= Getting started

== In your own crate

Replace the `v8` dependency with `v8x` and pick an engine feature:

```toml
[dependencies]
v8 = { package = "v8x", version = "158.0.0", features = ["quickjs"] }
```

Engine features are mutually exclusive; enable exactly one:

#table(
  columns: 3,
  [*feature*], [*engine*], [*notes*],
  [`quickjs`], [QuickJS-ng, vendored + static], [works everywhere, \~1 MB, fastest build],
  [`jsc`], [WebKit JSCOnly, built from source], [macOS; shippable, JIT-enabled],
  [`system_jsc`], [Apple's `JavaScriptCore.framework`], [macOS; zero engine bytes in your binary],
)

Your code keeps using the `v8` crate API: `v8::Isolate`, `v8::Local`,
handle scopes, all of it. Nothing else changes.

== Under Deno

Patch the workspace instead, so `deno_core` and everything above it picks up
the swap:

```toml
# deno's workspace Cargo.toml
[patch.crates-io]
v8 = { package = "v8x", version = "158.0.0", features = ["jsc"] }
```

```sh
cargo build -p deno
```

`deno_core` compiles unchanged; the resulting binary runs your JS on the
engine you selected.

== macOS note: JIT entitlements

JavaScriptCore's JIT needs permission to allocate executable memory. Binaries
using the `jsc` backend must be codesigned with the JIT entitlement:

```sh
codesign -s - -f --entitlements tools/jit-entitlements.plist ./your-binary
```

The `system_jsc` and `quickjs` backends don't need this. The test harness in
the v8x repo does it automatically.

== Building from the repo

```sh
git clone --recursive https://github.com/littledivy/v8x
cd v8x
cargo build --no-default-features --features quickjs   # no engine build step
cargo build --features jsc                             # builds WebKit from source
```

The first vendored JSC build compiles WebKit's JSCOnly target, which takes a
while. QuickJS builds in seconds.
