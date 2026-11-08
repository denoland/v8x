#import "./shim/html.typ": *

#set document(
  title: "v8x",
  description: "v8x makes rusty_v8 engine agnostic: run deno_core and Deno unchanged on JavaScriptCore or QuickJS.",
)

#show: html-shim

= v8x

`v8x` makes rusty_v8 engine agnostic. It is a drop-in replacement for the
`v8` crate that keeps the same Rust API and runs it on a different
JavaScript engine:

```diff
-v8 = "158.0.0"
+v8 = { package = "v8x", version = "158.0.0", features = ["jsc"] }
```

Anything built on rusty_v8, including `deno_core` and Deno itself, compiles
unchanged and runs on the engine you picked.

== Engines

#table(
  columns: 3,
  [*engine*], [*feature*], [*platforms*],
  [JavaScriptCore (WebKit JSCOnly, built from source)], [`jsc`], [macOS],
  [JavaScriptCore (Apple's system framework)], [`system_jsc`], [macOS],
  [QuickJS-ng (vendored, static)], [`quickjs`], [any],
)

One engine is active at a time. The usual reason to swap is binary size:

#table(
  columns: 3,
  [*engine*], [*deno binary*], [*engine size*],
  [V8 15.2], [78.7 MB], [\~40 MB static],
  [JSC (vendored)], [80.7 MB], [\~48 MB static],
  [system JSC], [54.2 MB], [0, ships with the OS],
  [quickjs-ng], [56.1 MB], [\~1 MB static],
)

== Progress

Two suites run unmodified against every backend: the rusty_v8 integration tests, and `deno_core`'s own test
suite under nextest. When a test fails, the backend gets fixed, not the
test.

#html.elem("div", attrs: (id: "chart", class: "chart"), "")
#html.elem("script", attrs: (src: "chart.js"), "")
#html.elem("script", "v8xChart(document.getElementById('chart'), 'status/')")
