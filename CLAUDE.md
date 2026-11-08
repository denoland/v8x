# v82jsc — agent playbook (test hill-climb)

`v82jsc` is a **v8 C-ABI compatibility layer** backed by pluggable JS engines.
The crate is literally named `v8` (version `158.0.0`) so it drops into Deno via
`[patch.crates-io]`. Backends (exactly one active at a time, feature-gated):

| backend id | features | engine | OS |
|---|---|---|---|
| `jsc` | `engine_jsc,vendor_jsc` | WebKit JSCOnly built from source (shippable) | macOS |
| `sys-jsc` | `system_jsc` | Apple's `JavaScriptCore.framework` | macOS |
| `quickjs` | `quickjs` | vendored quickjs-ng, static | any |

Backend implementation lives in **`src/jsc/`** and **`src/quickjs/`** — these
define the ~570 `v8__*` C-ABI symbols the vendored rusty_v8 Rust surface calls.

## Your job

Make **more tests pass**, one PR at a time, without regressing any that pass.
We track two suites per backend (6 cells total):

| suite | what runs |
|---|---|
| `rusty_v8` | vendored `vendor/rusty_v8/tests/*.rs`, unmodified, vs our shim |
| `deno_core` | `cargo nextest run -p deno_core` in a patched deno checkout |

(The full-deno node-compat + unit suites are dropped for now to keep CI cheap;
they can be re-added to `config.json` later — `run.mjs` still supports them.)

The matrix + suite definitions are in **`tests/harness/config.json`** (single
source of truth). Never edit the vendored test files or Deno's tests — they run
**as-is**. You fix the *backend*, not the test.

## The ratchet (how progress is locked in)

Each cell has a baseline of known-passing tests:

```
tests/status/baselines/<backend>/<suite>.txt   # one passing test name per line
```

- CI on a PR runs `run.mjs <suite> <backend> --check`. It is **red** if any
  baselined test now fails/vanishes (regression) **or** if new tests pass that
  aren't in the baseline yet.
- So every PR that makes tests pass **must also update the baseline** (below).
- `tests/status/report.json` and `tests/status/history.jsonl` are **CI-generated** — never
  hand-edit them. They aggregate the baselines for the dashboard.

## Local loop

Prereqs: Node 22+, Rust stable. macOS for the JSC backends.

### rusty_v8 (cheapest — start here)

```bash
# rusty_v8 on quickjs (no WebKit build, fastest feedback):
node tests/harness/run.mjs rusty_v8 quickjs

# see which tests FAIL (and which whole targets fail to link — those score 0):
cat tests/status/.runs/quickjs__rusty_v8.json | jq '.failing'
```

A test target that won't **link** (missing `v8__*`/C-ABI symbol) counts as 0
passing for its **whole file** — the biggest files are the biggest payoffs.
Find the undefined symbols from the build error, define them in `src/<engine>/`
(no-op/stub is fine to start; the test then fails instead of blocking the link),
rebuild. macOS `ld64` dead-strips *unreferenced* undefined symbols, but a symbol
a test actually references blocks the link → 0 for that file.

**Highest-leverage unlock right now:** `test_api.rs` (hundreds of tests) and
`test_cppgc` don't link because they reference two unimplemented subsystems:
- **inspector protocol** — ~40 `crdtp__*` fns (CreateResponse, DispatchResponse,
  Dispatchable, Serializable, vec_u8, …), declared in the vendored rusty_v8
  binding, undefined in our shim.
- **cppgc** (Oilpan) — `cppgc__Member__{Get,Assign}`, `cppgc__Visitor__Trace__Member`,
  `v8__Isolate__RequestGarbageCollectionForTesting`.
Landing those (even as safe non-crashing stubs) makes `test_api` link and surface
its hundreds across all 3 engines — likely the single biggest jump on the
dashboard. Tracked in #2 (assigned to divybot).

### deno_core (needs a deno checkout)

```bash
# clone + patch deno exactly like CI (tools/deno/DENO_REF pins the commit):
DENO_REF=$(cat tools/deno/DENO_REF)
git -C ../deno init -q && git -C ../deno fetch --depth 1 \
  https://github.com/denoland/deno "$DENO_REF" && git -C ../deno checkout -q FETCH_HEAD
sed "s#/Users/divy/gh/v82jsc#$PWD#g" tools/deno/deno-jsc-integration.patch | git -C ../deno apply --3way -
# select your backend's v8 features in the patch's [patch.crates-io] v8 line:
perl -0pi -e 's/"engine_jsc", "vendor_jsc"/"quickjs"/g' ../deno/Cargo.toml   # example

# runs `cargo nextest run -p deno_core` against our shim (no full deno binary):
node tests/harness/run.mjs deno_core quickjs --deno-dir=../deno
```

On macOS (`jsc`/`sys-jsc`) the runner auto-codesigns test binaries with
`tools/jit-entitlements.plist` so JSC's JIT can allocate executable memory.

## Submitting a fix (PR rules)

1. Implement the backend fix in `src/jsc/` or `src/quickjs/`.
2. Re-run the affected cell and **record the new passing set**:
   ```bash
   node tests/harness/run.mjs <suite> <backend> --update
   ```
   This rewrites `tests/status/baselines/<backend>/<suite>.txt` (sorted, deduped).
3. Confirm the ratchet holds and you didn't regress another cell:
   ```bash
   node tests/harness/run.mjs <suite> <backend> --check
   ```
4. Commit the **source fix + the baseline file** together. One PR should touch
   **one cell's baseline** (`tests/status/baselines/<backend>/<suite>.txt`) so parallel
   agents never collide. Do **not** touch `report.json`, `history.jsonl`, or any
   file under `vendor/` test dirs.
5. Open the PR. CI re-runs `--check` for your cell; green = mergeable.

## Don't

- Don't modify vendored rusty_v8 tests or Deno's test files to make them pass.
- Don't edit `tests/status/report.json` / `tests/status/history.jsonl` (CI owns them).
- Don't lower a baseline to dodge a regression — fix the regression.
- Don't enable a backend's feature alongside another (they collide at link).

## Docs (`site/`) — keep them true

The public docs (`site/*.typ`, published to Pages) describe the state of the
backends. **Any PR that changes what's true must update the docs in the same
PR**: a new feature flag, a platform gained or lost, a subsystem landing
(inspector, snapshots, wasm…), size numbers, or workflow changes
(harness commands, ratchet rules). If your change makes a sentence on the
site wrong, fix the sentence. Rebuild locally with `make -C site all` to
check the pages compile.

Style rules (Divy's, non-negotiable):

- No em dashes anywhere. Plain, calm, declarative sentences; no punchy
  fragments, rhetorical questions, or cutesy section names.
- Code blocks contain real code only. Mappings go in tables, sequences in
  numbered lists, diagrams in SVG (`site/static/`, drawn to match the
  paper's figures). Never ASCII pseudo-diagrams with aligned columns/arrows.
- Lead with semantics (ownership, identity, state), not C-ABI/symbol-count
  mechanics. Short prose; figure or code first.
- Only document what works today. No aspirational platforms or features.

## Reference

- Harness internals: `tests/harness/lib.mjs`, `run.mjs`, `aggregate.mjs`.
- Dashboard: `docs/index.html` (GitHub Pages, served at `/status/`). Reads
  `report.json` + history.
- Public docs site: `site/*.typ` (Typst → HTML, littledivy.com-style shim in
  `site/shim/html.typ`). `tools/build-site.sh` assembles docs + dashboard into
  `_site/`; both `pages.yml` and ci.yml's `report` job call it. Local preview:
  `make -C site serve`.
- CI: `.github/workflows/ci.yml` (`check` = rusty_v8, `deno_core` = deno_core
  suite, `report` = single-writer aggregator on main), `pages.yml` (dashboard).
- Deno pin + integration patch: `tools/deno/DENO_REF`,
  `tools/deno/deno-jsc-integration.patch`.
