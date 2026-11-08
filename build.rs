use std::env;
use std::path::{Path, PathBuf};

/// Emit `$OUT_DIR/bc_embed.rs` defining `EMBEDDED_BC`. When `V82JSC_BC_BLOB`
/// points at a packed bytecode blob (see `module.rs` for the format), its bytes
/// are `include_bytes!`'d straight into the binary so startup reads compiled
/// module bytecode from memory instead of the on-disk cache. Absent the env
/// var, `EMBEDDED_BC` is empty and the disk cache path is used as before.
fn emit_bc_embed() {
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let dst = out_dir.join("bc_embed.rs");
  println!("cargo:rerun-if-env-changed=V82JSC_BC_BLOB");
  let body = match env::var_os("V82JSC_BC_BLOB") {
    Some(p) if PathBuf::from(&p).is_file() => {
      let p = PathBuf::from(&p);
      println!("cargo:rerun-if-changed={}", p.display());
      format!(
        "pub static EMBEDDED_BC: &[u8] = include_bytes!(r\"{}\");",
        p.display()
      )
    }
    _ => "pub static EMBEDDED_BC: &[u8] = &[];".to_string(),
  };
  std::fs::write(&dst, body).unwrap();
}

fn emit_vendor_rerun_inputs(manifest_dir: &Path) {
  let patches_dir = manifest_dir.join("patches");
  println!("cargo:rerun-if-changed={}", patches_dir.display());
  let Ok(entries) = std::fs::read_dir(&patches_dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_file() {
      println!("cargo:rerun-if-changed={}", path.display());
    }
  }
}

fn emit_quickjs_cache_tag(manifest_dir: &Path) {
  const PRIME: u64 = 0x0000_0100_0000_01B3;
  const SOURCES: &[&str] = &[
    "quickjs.c",
    "quickjs.h",
    "quickjs-atom.h",
    "quickjs-opcode.h",
    "libregexp.c",
    "libregexp.h",
    "libunicode.c",
    "libunicode.h",
    "cutils.c",
    "cutils.h",
    "dtoa.c",
    "dtoa.h",
  ];

  let quickjs_dir = manifest_dir.join("vendor/quickjs-ng");
  let mut hash = 0xcbf2_9ce4_8422_2325u64;
  for relative in SOURCES {
    let path = quickjs_dir.join(relative);
    let Ok(bytes) = std::fs::read(&path) else {
      continue;
    };
    for byte in relative.bytes().chain(bytes) {
      hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    println!("cargo:rerun-if-changed={}", path.display());
  }
  println!("cargo:rustc-env=V82JSC_QUICKJS_CACHE_TAG={hash:016x}");
}

fn main() {
  let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

  // Init the pinned rusty_v8 submodule + apply our 2 patches BEFORE compile:
  // src/lib.rs `#[path]`-includes its modules, so the vendored Rust API surface
  // must be materialized for either backend. (Engines are set up separately,
  // only for the QuickJS path.)
  setup_vendor(&manifest_dir, "rusty_v8");

  // The vendored crate's `binding.rs` does
  // `include!(env!("RUSTY_V8_SRC_BINDING_PATH"))` to pull in the bindgen
  // output (extern decls + SIZE consts). We point it at the pre-generated
  // bindings for this target. The C ABI symbols are *defined* by our own
  // engine shim (linked below); only the declarations come from here.
  //
  // The files in gen/ are unmodified upstream rusty_v8 v158.0.0 release
  // assets. Their content varies only by OS family — the per-arch and
  // debug/release/simdutf variants upstream publishes are byte-identical
  // within one OS (verified; see gen/README.md) — so one file per OS family
  // covers every target: mangled C++ `link_name`s (Itanium `_Z..` with a
  // leading underscore on Apple, MSVC `?..` on Windows) and enum repr types
  // (c_int on MSVC, c_uint elsewhere).
  let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
  let gen_file = if env::var("CARGO_CFG_TARGET_VENDOR").as_deref()
    == Ok("apple")
  {
    "gen/src_binding_debug_aarch64-apple-darwin.rs"
  } else if target_os == "windows" && target_env == "msvc" {
    "gen/src_binding_release_x86_64-pc-windows-msvc.rs"
  } else {
    // Itanium mangling without the Apple underscore: linux-gnu/musl, the BSDs,
    // android, windows-gnu.
    "gen/src_binding_debug_x86_64-unknown-linux-gnu.rs"
  };
  let binding_path = manifest_dir.join(gen_file);
  println!(
    "cargo:rustc-env=RUSTY_V8_SRC_BINDING_PATH={}",
    binding_path.display()
  );
  println!("cargo:rerun-if-changed={}", binding_path.display());

  emit_bc_embed();

  // --- JSC backend: generate full FFI bindings from the SDK header. ---
  // `src/jsc_sys.rs` `include!`s the output, so the complete JavaScriptCore
  // C API is available without hand-written externs.
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some() && target_os != "macos"
  {
    panic!(
      "the JSC backends (features `jsc`/`engine_jsc`/`system_jsc`) are \
       macOS-only; build with `--no-default-features --features quickjs` \
       on {target_os}"
    );
  }
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some() {
    generate_jsc_bindings();
  }

  // --- JSC backend: vendored WebKit JSCOnly build, or system framework ---
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some()
    && env::var_os("CARGO_FEATURE_VENDOR_JSC").is_some()
  {
    build_vendored_jsc(&manifest_dir);
    return;
  }

  #[cfg(target_os = "macos")]
  if env::var_os("CARGO_FEATURE_ENGINE_JSC").is_some() {
    println!("cargo:rustc-link-lib=framework=JavaScriptCore");
    // `jsc_version_string` reads the JavaScriptCore bundle's version via
    // CoreFoundation (CFBundle*/CFString*/CFRelease). Those symbols are only
    // pulled in once a test target references `v8__V8__GetVersion` (the small
    // suites dead-strip them), so link CoreFoundation explicitly or the
    // `test_api` target fails to link on the system-framework backend.
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    // lld with -nodefaultlibs doesn't search the SDK, where macOS now keeps
    // the .tbd stubs for system libs like iconv (the .dylib files were moved
    // into the dyld shared cache). Add the SDK lib dir so `-liconv` resolves.
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
    {
      if let Ok(sdk) = String::from_utf8(out.stdout) {
        let sdk = sdk.trim();
        if !sdk.is_empty() {
          println!("cargo:rustc-link-search=native={sdk}/usr/lib");
        }
      }
    }
  }

  // --- QuickJS-ng backend: compile + statically link the vendored sources ---
  if env::var_os("CARGO_FEATURE_LINK_QUICKJS").is_some() {
    // Init the pinned quickjs-ng + WAMR submodules and apply our patches
    // (idempotent). Skipped when both engines are driven from prebuilt trees.
    if env::var_os("QUICKJS_NG_LIB_DIR").is_none()
      || env::var_os("WAMR_LIB_DIR").is_none()
    {
      setup_vendor(&manifest_dir, "quickjs");
    }
    emit_quickjs_cache_tag(&manifest_dir);
    build_quickjs(&manifest_dir);
    // WebAssembly engine: build the vendored WAMR (interpreter-only) static
    // lib and link it; the WebAssembly.* JS API is implemented over its
    // wasm-c-api in src/quickjs/wasm.rs.
    build_wamr(&manifest_dir);
  }
}

/// Init the pinned vendor submodules and apply our patch files on top.
/// Pure-Rust port of tools/setup_vendor.sh (kept for manual use — change both
/// together) so a fresh checkout builds without bash, notably on Windows.
/// Idempotent: `.v8x-patches/` stamp files skip patches whose checksum hasn't
/// changed, and an applied patch is detected via `git apply --reverse --check`.
fn setup_vendor(manifest_dir: &Path, mode: &str) {
  // A published crates.io package ships the vendored sources already
  // materialized and patched (see the `include` list in Cargo.toml), with no
  // git metadata to init submodules or apply patches against. Detect that and
  // skip the whole dance: a git checkout has a `.git` entry, an extracted
  // `.crate` tarball does not.
  if !manifest_dir.join(".git").exists() {
    return;
  }
  apply_patch_series(manifest_dir, "vendor/rusty_v8", "rusty_v8");
  ensure_rusty_v8_icu(manifest_dir);
  if mode == "quickjs" {
    apply_patch_series(manifest_dir, "vendor/quickjs-ng", "quickjs");
    apply_patch_series(manifest_dir, "vendor/wamr", "wamr");
    // WAMR's CMake driver has no upstream counterpart; copy it in.
    let dst_dir = manifest_dir.join("vendor/wamr/v82jsc");
    std::fs::create_dir_all(&dst_dir).unwrap();
    std::fs::copy(
      manifest_dir.join("patches/wamr-v82jsc-CMakeLists.txt"),
      dst_dir.join("CMakeLists.txt"),
    )
    .expect("failed to copy WAMR CMakeLists driver");
  }
  emit_vendor_rerun_inputs(manifest_dir);
}

fn run_git(cwd: &Path, args: &[&str]) -> bool {
  std::process::Command::new("git")
    .args(args)
    .current_dir(cwd)
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

fn remove_stale_submodule_dir(path: &Path) {
  for attempt in 1..=30 {
    match std::fs::remove_dir_all(path) {
      Ok(()) => return,
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
      Err(_) if attempt < 30 => {
        if attempt == 1 {
          println!(
            "cargo:warning=waiting to clear stale Cargo submodule state: {}",
            path.display()
          );
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
      }
      Err(err) => {
        panic!("failed to remove stale {}: {err}", path.display())
      }
    }
  }
}

fn reset_repaired_submodule(root: &Path, sub: &str) {
  let sub_dir = root.join(sub);
  assert!(
    run_git(
      &sub_dir,
      &["-c", "core.autocrlf=false", "reset", "--hard", "HEAD"]
    ),
    "failed to reset repaired Cargo submodule {sub}"
  );
  assert!(
    run_git(&sub_dir, &["clean", "-ffdx"]),
    "failed to clean repaired Cargo submodule {sub}"
  );
}

fn initialize_submodule(root: &Path, sub: &str) {
  let update_cfg = format!("submodule.{sub}.update=checkout");
  let update = || {
    // core.autocrlf=false: the patches are made against LF trees; a Windows
    // clone with the Git for Windows default (autocrlf=true) would otherwise
    // check the submodule out with CRLF and every patch context would miss.
    // (-c propagates to the spawned clone/checkout via GIT_CONFIG_PARAMETERS.)
    run_git(
      root,
      &[
        "-c",
        "core.autocrlf=false",
        "-c",
        &update_cfg,
        "submodule",
        "update",
        "--init",
        sub,
      ],
    )
  };
  if update() {
    if root.join(".cargo-ok").is_file() {
      reset_repaired_submodule(root, sub);
    }
    return;
  }

  // Cargo marks its managed git checkouts with `.cargo-ok`. Restored Cargo
  // caches can retain a populated submodule worktree after its `.git` file or
  // module metadata has gone stale, making `git submodule update` refuse the
  // non-empty destination. Only in a Cargo-owned checkout, discard both stale
  // halves and retry from the pinned gitlink.
  assert!(
    root.join(".cargo-ok").is_file(),
    "git submodule update --init {sub} failed"
  );
  println!("cargo:warning=repairing stale Cargo submodule checkout: {sub}");
  for stale in [root.join(sub), root.join(".git/modules").join(sub)] {
    remove_stale_submodule_dir(&stale);
  }
  let index_lock = root.join(".git/modules").join(sub).join("index.lock");
  for attempt in 1..=3 {
    if update() {
      reset_repaired_submodule(root, sub);
      return;
    }
    // Git for Windows may return before a failed submodule helper has removed
    // the lock it recreated in the freshly initialized module directory.
    // Once the update has failed, this lock is stale and safe to clear inside
    // the already-guarded Cargo checkout repair path.
    if index_lock.is_file() {
      println!("cargo:warning=clearing stale Cargo submodule lock: {sub}");
      let _ = std::fs::remove_file(&index_lock);
    }
    if attempt < 3 {
      std::thread::sleep(std::time::Duration::from_millis(100));
    }
  }
  panic!(
    "git submodule update --init {sub} failed after clearing Cargo cache state"
  );
}

/// Apply every patches/<prefix>-NN-*.patch onto a submodule, ordered like
/// `sort -V` (numerically by NN, then by name). Initializes the submodule
/// first if its working tree is absent.
fn apply_patch_series(root: &Path, sub: &str, prefix: &str) {
  let sub_dir = root.join(sub);
  if !sub_dir.join(".git").exists() {
    initialize_submodule(root, sub);
  }
  let stamp_dir = sub_dir.join(".v8x-patches");
  std::fs::create_dir_all(&stamp_dir).unwrap();

  let mut patches: Vec<(u64, String, PathBuf)> = Vec::new();
  for entry in std::fs::read_dir(root.join("patches")).unwrap().flatten() {
    let name = entry.file_name().to_string_lossy().into_owned();
    let Some(rest) = name.strip_prefix(&format!("{prefix}-")) else {
      continue;
    };
    if !name.ends_with(".patch")
      || !rest.starts_with(|c: char| c.is_ascii_digit())
    {
      continue;
    }
    let num: u64 = rest
      .chars()
      .take_while(char::is_ascii_digit)
      .collect::<String>()
      .parse()
      .unwrap();
    patches.push((num, name, entry.path()));
  }
  patches.sort();

  for (_, name, patch) in patches {
    let contents = std::fs::read(&patch).unwrap();
    // Same format tools/setup_vendor.sh writes (`cksum < patch`), so stamps
    // written by either implementation are honored by the other. That matters:
    // patches applied long ago may no longer probe as applied (later patches
    // shift their context past what `git apply --reverse --check` tolerates),
    // so invalidating existing stamps would fail such trees spuriously.
    let checksum = format!("{} {}", posix_cksum(&contents), contents.len());
    let stamp = stamp_dir.join(&name);
    if std::fs::read_to_string(&stamp)
      .map(|s| s.trim_end() == checksum)
      .unwrap_or(false)
    {
      continue;
    }
    // Already absolute (root is CARGO_MANIFEST_DIR). Deliberately NOT
    // canonicalize(): on Windows that yields a \\?\-prefixed path git rejects.
    let patch_str = patch.to_str().unwrap();
    let applied = run_git(
      &sub_dir,
      &["apply", "--reverse", "--check", patch_str],
    ) || run_git(&sub_dir, &["apply", patch_str])
      || patch_fallback(root, sub, &patch);
    assert!(applied, "failed to apply patches/{name} onto {sub}");
    std::fs::write(&stamp, format!("{checksum}\n")).unwrap();
  }
}

/// POSIX cksum(1): CRC-32 (poly 0x04C11DB7, MSB-first, init 0) over the data
/// followed by the length as minimal little-endian bytes, complemented.
fn posix_cksum(bytes: &[u8]) -> u32 {
  fn step(mut crc: u32, b: u8) -> u32 {
    crc ^= u32::from(b) << 24;
    for _ in 0..8 {
      crc = if crc & 0x8000_0000 != 0 {
        (crc << 1) ^ 0x04C1_1DB7
      } else {
        crc << 1
      };
    }
    crc
  }
  let mut crc = bytes.iter().fold(0u32, |c, &b| step(c, b));
  let mut len = bytes.len() as u64;
  while len != 0 {
    crc = step(crc, (len & 0xff) as u8);
    len >>= 8;
  }
  !crc
}

/// `git apply` rejected the patch; retry with patch(1), which fuzzes offsets,
/// and treat "previously applied" as success. Not used for git binary deltas:
/// GNU patch doesn't understand them and *silently skips* those sections while
/// exiting 0 on the text hunks — a half-applied patch with a written stamp.
fn patch_fallback(root: &Path, sub: &str, patch: &Path) -> bool {
  if std::fs::read(patch)
    .map(|c| {
      c.windows(b"GIT binary patch".len())
        .any(|w| w == b"GIT binary patch")
    })
    .unwrap_or(true)
  {
    return false;
  }
  let run = |extra: &[&str]| {
    std::process::Command::new("patch")
      .args(["--batch", "--forward", "-p1", "-d", sub])
      .args(extra)
      .arg("-i")
      .arg(patch)
      .current_dir(root)
      .output()
  };
  let Ok(dry) = run(&["--dry-run"]) else {
    return false; // no patch(1) on this system
  };
  if dry.status.success() {
    return run(&[]).map(|o| o.status.success()).unwrap_or(false);
  }
  let out = String::from_utf8_lossy(&dry.stdout).into_owned()
    + &String::from_utf8_lossy(&dry.stderr);
  if out.contains("previously applied") {
    println!(
      "cargo:warning={} may already be applied",
      patch.display()
    );
    return true;
  }
  false
}

/// rusty_v8's tests embed third_party/icu/common/icudtl.dat at compile time.
/// Keep the real pinned Chromium ICU data available; the 10 MiB blob is not
/// committed here, so init the nested submodule when the file is missing or
/// truncated.
fn ensure_rusty_v8_icu(root: &Path) {
  let rusty_v8 = root.join("vendor/rusty_v8");
  let dat = rusty_v8.join("third_party/icu/common/icudtl.dat");
  let size = std::fs::metadata(&dat).map(|m| m.len()).unwrap_or(0);
  if size < 1_048_576 {
    let _ = std::fs::remove_dir_all(rusty_v8.join("third_party/icu"));
    assert!(
      run_git(
        &rusty_v8,
        &["submodule", "update", "--init", "third_party/icu"]
      ),
      "git submodule update --init third_party/icu failed in vendor/rusty_v8"
    );
  }
}

/// Build the vendored wasm-micro-runtime (WAMR) as an interpreter-only static
/// library via CMake and link it. Backs the QuickJS backend's `WebAssembly`.
fn build_wamr(manifest_dir: &std::path::Path) {
  if let Some(dir) = env::var_os("WAMR_LIB_DIR") {
    println!(
      "cargo:rustc-link-search=native={}",
      PathBuf::from(dir).display()
    );
    println!("cargo:rustc-link-lib=static=vmlib");
    return;
  }
  let src = manifest_dir.join("vendor/wamr/v82jsc");
  let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("wamr-build");
  // Wipe any stale cmake cache so flag changes (notably the HW-bound-check
  // disable) always take effect.
  let _ = std::fs::remove_dir_all(&out);
  std::fs::create_dir_all(&out).unwrap();
  let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
  let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
  let cmake = |args: &[&str]| {
    let status = std::process::Command::new("cmake")
      .args(args)
      .current_dir(&out)
      .status()
      .expect("cmake not found — needed to build WAMR");
    assert!(status.success(), "cmake step failed: {args:?}");
  };
  let mut configure_args = vec![
    "-DCMAKE_BUILD_TYPE=Release".to_string(),
    "-DCMAKE_POLICY_VERSION_MINIMUM=3.5".to_string(),
    // WAMR's hardware bound-check installs SIGSEGV/SIGBUS handlers that fight
    // Rust's stack-overflow guard (instant abort). Force software checks.
    "-DWAMR_DISABLE_HW_BOUND_CHECK=1".to_string(),
    "-DWAMR_DISABLE_STACK_HW_BOUND_CHECK=1".to_string(),
  ];
  if let Some(target) = match target_arch.as_str() {
    "aarch64" => Some("AARCH64"),
    "x86_64" => Some("X86_64"),
    "x86" => Some("X86_32"),
    _ => None,
  } {
    // CMake reports the host architecture for some generators and spells
    // native Windows ARM64 differently than WAMR expects. Cargo's target is
    // authoritative for both native and cross builds.
    configure_args.push(format!("-DWAMR_BUILD_TARGET={target}"));
  }
  if target_os == "windows" && target_env == "msvc" && target_arch == "aarch64"
  {
    // Visual Studio's ASM_MASM integration invokes x86 MASM even for native
    // ARM64 projects and ignores WAMR's armasm64 compiler override. Use WAMR's
    // portable native-call bridge so the ARM64 build has no assembler input.
    configure_args.push("-DWAMR_BUILD_INVOKE_NATIVE_GENERAL=1".to_string());
  }
  if target_os == "windows" && target_env == "msvc" {
    // Keep WAMR's C runtime linkage aligned with Rust. In particular, Deno
    // enables crt-static and otherwise gets a mixture of /MT and /MD objects.
    let target_features =
      env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let runtime = if target_features.split(',').any(|f| f == "crt-static") {
      "MultiThreaded"
    } else {
      "MultiThreadedDLL"
    };
    configure_args.push("-DCMAKE_POLICY_DEFAULT_CMP0091=NEW".to_string());
    configure_args.push(format!("-DCMAKE_MSVC_RUNTIME_LIBRARY={runtime}"));
  }
  configure_args.push(src.to_string_lossy().into_owned());
  let configure_arg_refs = configure_args
    .iter()
    .map(String::as_str)
    .collect::<Vec<_>>();
  cmake(&configure_arg_refs);
  // `--config` matters only for multi-config generators (the Visual Studio
  // default on Windows, which emits into a Release/ subdir); single-config
  // generators ignore it.
  cmake(&["--build", ".", "--config", "Release", "-j", "4"]);
  println!("cargo:rustc-link-search=native={}", out.display());
  println!(
    "cargo:rustc-link-search=native={}",
    out.join("Release").display()
  );
  println!("cargo:rustc-link-lib=static=vmlib");
  if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    // win_socket.c / win_thread.c pull in winsock; the #pragma comment(lib)
    // in the objects covers MSVC link.exe, but be explicit for lld-link.
    println!("cargo:rustc-link-lib=ws2_32");
  }
  println!("cargo:rerun-if-changed={}", src.display());
}

/// Build JavaScriptCore from the vendored WebKit (JSCOnly port) and link it.
/// Override the build with `JSC_VENDOR_BUILD_DIR` pointing at a prebuilt
/// `WebKitBuild/JSCOnly/Release` (containing `lib/`).
fn build_vendored_jsc(manifest_dir: &std::path::Path) {
  let webkit = manifest_dir.join("vendor/webkit");
  let build_dir = env::var_os("JSC_VENDOR_BUILD_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| webkit.join("WebKitBuild/JSCOnly/Release"));
  let lib_dir = build_dir.join("lib");

  // Bundled = STATIC: the JSCOnly port with -DENABLE_STATIC_JSC=ON emits
  // libJavaScriptCore.a + libWTF.a + libbmalloc.a; we link them into the
  // binary so it's self-contained (no dylib, no rpath).
  let jsc_a = lib_dir.join("libJavaScriptCore.a");
  let prebuilt =
    jsc_a.exists() || env::var_os("JSC_VENDOR_BUILD_DIR").is_some();
  if prebuilt {
    // A PREBUILT lib archive is in place (CI downloads the WebKit static-lib
    // release — see .github/workflows/webkit-release.yml). Still apply the
    // source patches so the glue (native_modules.cpp) compiles against the
    // patched headers; skip the multi-hour build.
    let _ = &webkit;
    let status = std::process::Command::new("bash")
      .arg(manifest_dir.join("tools/setup_webkit.sh"))
      .arg("--patches-only")
      .current_dir(manifest_dir)
      .status();
    match status {
      Ok(s) if s.success() => {}
      other => panic!("tools/setup_webkit.sh --patches-only failed: {other:?}"),
    }
  } else {
    // tools/setup_webkit.sh inits the pinned submodule, applies the patches,
    // and runs the static JSCOnly build — everything needed for a fresh tree.
    let _ = &webkit;
    let status = std::process::Command::new("bash")
      .arg(manifest_dir.join("tools/setup_webkit.sh"))
      .current_dir(manifest_dir)
      .status();
    match status {
      Ok(s) if s.success() => {}
      other => {
        panic!("tools/setup_webkit.sh (WebKit JSC build) failed: {other:?}")
      }
    }
  }

  // Compile the native ES-module glue (src/jsc/native_modules.cpp) against the
  // vendored WebKit private headers and link it. Replaces the rewrite_es_module
  // string rewriter with real JSModuleRecords.
  build_native_modules_glue(manifest_dir, &webkit, &build_dir);

  println!("cargo:rustc-link-search=native={}", lib_dir.display());
  // JavaScriptCore + JavaScriptCoreJIT are split cmake targets; link both as
  // normal static libs (like the `jsc` CLI does) and repeat to satisfy their
  // cyclic references. NOTE: the offlineasm LLInt/IPInt assembly objects in
  // these archives have MH_SUBSECTIONS_VIA_SYMBOLS cleared by tools/
  // setup_webkit.sh so the deno binary's `-Wl,-dead_strip` cannot strip the
  // computed-jump-only WASM opcode handlers (else WASM runs garbage). See the
  // comment there.
  println!("cargo:rustc-link-lib=static=JavaScriptCore");
  let jit_a = lib_dir.join("libJavaScriptCoreJIT.a");
  if jit_a.exists() {
    println!("cargo:rustc-link-lib=static=JavaScriptCoreJIT");
    // second pass: JIT <-> core have cyclic references
    println!("cargo:rustc-link-lib=static=JavaScriptCore");
    println!("cargo:rustc-link-lib=static=JavaScriptCoreJIT");
  }
  println!("cargo:rustc-link-lib=static=WTF");
  println!("cargo:rustc-link-lib=static=bmalloc");
  println!("cargo:rustc-link-lib=c++");
  println!("cargo:rerun-if-changed={}", jsc_a.display());

  #[cfg(target_os = "macos")]
  {
    // ICU + the system frameworks WTF/JSC depend on.
    println!("cargo:rustc-link-lib=icucore");
    for fw in ["CoreFoundation", "Foundation", "Security"] {
      println!("cargo:rustc-link-lib=framework={fw}");
    }
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
    {
      if let Ok(sdk) = String::from_utf8(out.stdout) {
        println!("cargo:rustc-link-search=native={}/usr/lib", sdk.trim());
      }
    }
  }
}

/// Compile `src/jsc/native_modules.cpp` (the native JSModuleRecord glue) against
/// the vendored WebKit's private + derived headers, archive it, and link it. The
/// glue exposes `v82jsc_module_*` C functions the JSC module shims call.
///
/// We mirror the include set JSC's own unified sources use (extracted from
/// `compile_commands.json`): the JavaScriptCore.hmap header map resolves the
/// unprefixed parser internals (`ModuleAnalyzer.h`, `Nodes.h`, ...) that aren't
/// in PrivateHeaders. config.h is included first by the .cpp itself, so no PCH
/// is needed. Apple clang (via `xcrun`) — NOT a PATH `clang++` which may be a
/// mismatched LLVM that mishandles the SDK headers.
fn build_native_modules_glue(
  manifest_dir: &std::path::Path,
  webkit: &std::path::Path,
  build_dir: &std::path::Path,
) {
  // Glue translation units, archived together: native_modules.cpp (the
  // JSModuleRecord module system), bytecode.cpp (JSC bytecode cache), and
  // introspect.cpp (Proxy handler / Promise state / iterator preview).
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let archive = out_dir.join("libv82jsc_native_modules.a");
  let units = ["native_modules.cpp", "bytecode.cpp", "introspect.cpp"];

  let sdk = String::from_utf8(
    std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
      .expect("xcrun --show-sdk-path failed")
      .stdout,
  )
  .expect("sdk path not utf8");
  let sdk = sdk.trim();

  let b = build_dir.to_str().unwrap();
  let inc = |p: &str| format!("-I{b}/{p}");
  let mut objs: Vec<PathBuf> = Vec::new();
  for unit in units {
    let src = manifest_dir.join("src/jsc").join(unit);
    let obj = out_dir.join(unit).with_extension("o");
    objs.push(obj.clone());
    let status = std::process::Command::new("xcrun")
      .args(["clang++", "-c"])
      .arg(&src)
      .arg("-o")
      .arg(&obj)
      .args([
        "-DBUILDING_JSCONLY__",
        "-DBUILDING_JavaScriptCore",
        "-DBUILDING_WEBKIT=1",
        "-DBUILDING_WITH_CMAKE=1",
        "-DHAVE_CONFIG_H=1",
        "-DPAS_BMALLOC=1",
        "-DSTATICALLY_LINKED_WITH_WTF",
        "-DSTATICALLY_LINKED_WITH_bmalloc",
        "-DU_DISABLE_RENAMING=1",
        "-D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_EXTENSIVE",
        "-DNDEBUG",
      ])
      .arg(inc("JavaScriptCore/Headers"))
      .arg(inc("JavaScriptCore/PrivateHeaders"))
      .arg(format!("-I{b}"))
      .arg(inc("HeaderMaps/JavaScriptCore.hmap"))
      .arg(inc("JavaScriptCore/PrivateHeaders/JavaScriptCore"))
      .arg(format!("-I{}/Source/JavaScriptCore", webkit.display()))
      .arg(inc("JavaScriptCore/DerivedSources"))
      .arg(inc("JavaScriptCore/DerivedSources/inspector"))
      .arg(inc("JavaScriptCore/DerivedSources/runtime"))
      .arg(inc("JavaScriptCore/DerivedSources/yarr"))
      .arg(inc("WTF/Headers"))
      .arg(inc("bmalloc/Headers"))
      .arg(inc("bmalloc/PrivateHeaders"))
      .args(["-isystem", &format!("{b}/ICU/Headers")])
      .args([
        "-std=c++2b",
        "-O3",
        "-fno-exceptions",
        "-fno-rtti",
        "-fvisibility=hidden",
        "-fvisibility-inlines-hidden",
        "-fPIC",
        "-ffp-contract=off",
        "-fno-slp-vectorize",
        "-arch",
        "arm64",
        "-Wno-everything",
      ])
      .args(["-isysroot", sdk])
      .status()
      .expect("xcrun clang++ not found — needed to build the JSC glue");
    assert!(status.success(), "{unit} compile failed");
    println!("cargo:rerun-if-changed={}", src.display());
  }

  // Archive the objects so the linker pulls them on demand (the Rust shims
  // reference the v82jsc_* symbols).
  let _ = std::fs::remove_file(&archive);
  let mut ar = std::process::Command::new("ar");
  ar.arg("crs").arg(&archive);
  for o in &objs {
    ar.arg(o);
  }
  assert!(
    ar.status().expect("ar failed").success(),
    "ar archiving failed"
  );

  println!("cargo:rustc-link-search=native={}", out_dir.display());
  println!("cargo:rustc-link-lib=static=v82jsc_native_modules");
}

/// Run bindgen over the SDK's JavaScriptCore C API umbrella header to produce
/// the complete set of declarations (`JSValueRef`, `JSEvaluateScript`,
/// `JSType`/`kJSType*`, `JSTypedArrayType`/`kJSTypedArrayType*`, ...). The
/// generated names are identical to the C names, so `src/jsc_sys.rs` just
/// `include!`s the output and the shim code keeps compiling.
fn generate_jsc_bindings() {
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let out_path = out_dir.join("jsc_bindings.rs");

  let sdk = String::from_utf8(
    std::process::Command::new("xcrun")
      .args(["--show-sdk-path"])
      .output()
      .expect("xcrun --show-sdk-path failed")
      .stdout,
  )
  .expect("sdk path not utf8");
  let sdk = sdk.trim();
  let frameworks = format!("{sdk}/System/Library/Frameworks");
  let header =
    format!("{frameworks}/JavaScriptCore.framework/Headers/JavaScript.h");

  // bindgen locates libclang via the `clang-sys` crate. On a stock Xcode
  // install it lives in the toolchain lib dir; point LIBCLANG_PATH there if
  // it isn't already set so the build works out of the box.
  if env::var_os("LIBCLANG_PATH").is_none() {
    if let Ok(out) = std::process::Command::new("xcrun")
      .args(["--find", "clang"])
      .output()
    {
      if let Ok(clang) = String::from_utf8(out.stdout) {
        // .../usr/bin/clang -> .../usr/lib
        if let Some(libdir) = PathBuf::from(clang.trim())
          .parent()
          .and_then(|p| p.parent())
          .map(|p| p.join("lib"))
        {
          if libdir.join("libclang.dylib").exists() {
            unsafe { env::set_var("LIBCLANG_PATH", &libdir) };
          }
        }
      }
    }
  }

  let bindings = bindgen::Builder::default()
    .header(&header)
    .clang_arg("-isysroot")
    .clang_arg(sdk)
    .clang_arg(format!("-F{frameworks}"))
    .allowlist_function("JS.*")
    .allowlist_type("JS.*|Opaque.*")
    .allowlist_var("kJS.*")
    .generate()
    .expect("bindgen failed to generate JavaScriptCore bindings");

  // Edition 2024 requires `extern` blocks to be `unsafe extern`. bindgen 0.70
  // still emits bare `extern "C" {`, so rewrite the block headers. (Function
  // pointer typedefs already use `unsafe extern "C" fn(...)` and are skipped
  // because the pattern below only matches a block-opening brace.)
  let src = bindings
    .to_string()
    .replace("extern \"C\" {", "unsafe extern \"C\" {");

  std::fs::write(&out_path, src).expect("failed to write jsc_bindings.rs");

  println!("cargo:rerun-if-changed={header}");
}

#[allow(dead_code)]
fn build_quickjs(manifest_dir: &std::path::Path) {
  // Honor a prebuilt tree first.
  if let Some(dir) = env::var_os("QUICKJS_NG_LIB_DIR") {
    println!(
      "cargo:rustc-link-search=native={}",
      PathBuf::from(dir).display()
    );
    println!("cargo:rustc-link-lib=static=quickjs");
    return;
  }
  let qjs = manifest_dir.join("vendor/quickjs-ng");
  let quickjs_c = qjs.join("quickjs.c");
  let quickjs_src =
    std::fs::read_to_string(&quickjs_c).expect("failed to read quickjs.c");
  assert!(
    quickjs_src.contains("v82jsc_global_var_obj"),
    "QuickJS patch series is missing quickjs-17-global-lexicals.patch"
  );
  // The four core sources matching upstream CMake `qjs_sources`.
  let sources = [
    "quickjs.c",
    "libregexp.c",
    "libunicode.c",
    "cutils.c",
    "dtoa.c",
  ];
  let mut build = cc::Build::new();
  build.include(&qjs);
  for s in sources {
    let p = qjs.join(s);
    if p.exists() {
      build.file(p);
    }
  }
  build
    .define("_GNU_SOURCE", None)
    // Real QuickJS ships with NDEBUG; it also drops the JS_FreeRuntime
    // gc_obj_list assert so a (temporary) refcount leak doesn't abort.
    .define("NDEBUG", None)
    .flag_if_supported("-Wno-implicit-fallthrough")
    .flag_if_supported("-Wno-sign-compare")
    .flag_if_supported("-Wno-unused-parameter")
    .flag_if_supported("-Wno-unused-but-set-variable")
    .flag_if_supported("-Wno-unused-variable")
    // Match quickjs-ng's CMake Release configuration.
    .opt_level(3);
  // Mirror upstream quickjs-ng's Windows/MSVC CMake configuration: C11 with
  // the (still "experimental") MSVC C11 atomics, and lean windows.h.
  if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    build
      .define("WIN32_LEAN_AND_MEAN", None)
      .define("_WIN32_WINNT", "0x0601")
      .define("_CRT_SECURE_NO_WARNINGS", None)
      .define("_CRT_NONSTDC_NO_DEPRECATE", None)
      .flag_if_supported("/std:c11")
      .flag_if_supported("/experimental:c11atomics");
  }
  build.compile("quickjs");
  println!("cargo:rerun-if-changed={}", qjs.display());
}
