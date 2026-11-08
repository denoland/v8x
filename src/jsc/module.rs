//! JSC-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Script compilation/execution is implemented for real via JSEvaluateScript:
//! a `Script`/`UnboundScript` handle simply carries the source-text JSValueRef,
//! and `Run` re-stringifies it and evaluates it in the current context.
//! ES-module-specific functions are inert because the JSC C API exposes no
//! module loader / linker.
#![allow(non_snake_case, unused)]

use std::mem::MaybeUninit;
use std::ptr;

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::{
  Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object,
  Primitive, RealIsolate, Script, String as V8String, UnboundModuleScript,
  UnboundScript, Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
  Location, ModuleStatus, ResolveModuleCallback, ResolveModuleCallbackRet,
  ResolveSourceCallback, StalledTopLevelAwaitMessage,
  SyntheticModuleEvaluationSteps, SyntheticModuleEvaluationStepsRet,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{
  CachedData, CompileOptions, NoCacheReason, Source,
};
use crate::support::{Maybe, MaybeBool, int};

#[repr(C)]
struct ModJSClassDefinition {
  version: std::os::raw::c_int,
  attributes: u32,
  className: *const std::os::raw::c_char,
  parentClass: JSClassRef,
  staticValues: *const std::os::raw::c_void,
  staticFunctions: *const std::os::raw::c_void,
  initialize: *const std::os::raw::c_void,
  finalize: *const std::os::raw::c_void,
  hasProperty: *const std::os::raw::c_void,
  getProperty: *const std::os::raw::c_void,
  setProperty: *const std::os::raw::c_void,
  deleteProperty: *const std::os::raw::c_void,
  getPropertyNames: *const std::os::raw::c_void,
  callAsFunction: *const std::os::raw::c_void,
  callAsConstructor: *const std::os::raw::c_void,
  hasInstance: *const std::os::raw::c_void,
  convertToType: *const std::os::raw::c_void,
}

struct SyntheticModule {
  ctx: JSGlobalContextRef,
  status: ModuleStatus,
  export_names: Vec<std::string::String>,

  eval_steps: Option<SyntheticModuleEvaluationSteps<'static>>,

  source: Option<std::string::String>,

  // The `//# sourceMappingURL=` magic-comment value (if any), extracted from the
  // original module source at compile. deno reads this via
  // UnboundModuleScript::GetSourceMappingURL to register native source maps.
  source_map_url: Option<std::string::String>,

  import_specifiers: Vec<std::string::String>,

  // Byte offset of each import's specifier literal (opening quote) in the
  // original source, parallel to import_specifiers. V8's ModuleRequest
  // source_offset; deno records it as `referrer_source_offset`.
  import_offsets: Vec<i32>,

  // Import attributes (`with { type: "json", ... }`) per import, parallel to
  // import_specifiers, extracted from source. JSC's record only exposes a coarse
  // type enum (json/wasm), so deno can't see `type: "text"/"bytes"/<custom>` or
  // arbitrary keys without this. Surfaced via ModuleRequest::GetImportAttributes.
  import_attributes: Vec<Vec<(std::string::String, std::string::String)>>,

  namespace: JSObjectRef,

  specifier: std::string::String,

  dependencies: Vec<*const Module>,

  is_async: bool,

  // Native JSC path (V82JSC_NATIVE_MODULES, vendored only): an opaque
  // Strong<JSModuleRecord>* from native_modules.cpp. When set, compile/link/
  // evaluate/namespace delegate to the real JSModuleRecord instead of the
  // rewrite_es_module string rewriter. null for synthetic + rewriter modules.
  native: *mut std::ffi::c_void,
}

// --- Native ES-module glue (src/jsc/native_modules.cpp). Vendored JSC only;
// the symbols live in libv82jsc_native_modules.a, linked by build.rs. ---
#[cfg(feature = "vendor_jsc")]
unsafe extern "C" {
  fn v82jsc_module_parse(
    ctx: JSContextRef,
    url: *const std::os::raw::c_char,
    src: *const std::os::raw::c_char,
    exc_out: *mut JSValueRef,
  ) -> *mut std::ffi::c_void;
  fn v82jsc_module_request_count(handle: *mut std::ffi::c_void) -> i32;
  fn v82jsc_module_request_at(
    handle: *mut std::ffi::c_void,
    i: i32,
    buf: *mut std::os::raw::c_char,
    cap: i32,
  ) -> i32;
  fn v82jsc_module_add_dependency(
    ctx: JSContextRef,
    parent: *mut std::ffi::c_void,
    spec: *const std::os::raw::c_char,
    dep: *mut std::ffi::c_void,
  ) -> bool;
  fn v82jsc_module_link(
    ctx: JSContextRef,
    handle: *mut std::ffi::c_void,
  ) -> bool;
  fn v82jsc_module_evaluate(
    ctx: JSContextRef,
    handle: *mut std::ffi::c_void,
  ) -> JSValueRef;
  fn v82jsc_module_namespace(
    ctx: JSContextRef,
    handle: *mut std::ffi::c_void,
  ) -> JSValueRef;
  fn v82jsc_module_release(handle: *mut std::ffi::c_void);
  fn v82jsc_module_status(handle: *mut std::ffi::c_void) -> i32;
  fn v82jsc_module_record_ptr(
    handle: *mut std::ffi::c_void,
  ) -> *mut std::ffi::c_void;
  fn v82jsc_module_request_attr_type(
    handle: *mut std::ffi::c_void,
    i: i32,
  ) -> i32;
  fn v82jsc_synthetic_create(
    ctx: JSContextRef,
    url: *const std::os::raw::c_char,
    names: *const *const std::os::raw::c_char,
    count: i32,
  ) -> *mut std::ffi::c_void;
  fn v82jsc_synthetic_set_export(
    ctx: JSContextRef,
    handle: *mut std::ffi::c_void,
    name: *const std::os::raw::c_char,
    value: JSValueRef,
  ) -> bool;
  // bytecode.cpp — JSC bytecode cache (fast startup).
  fn v82jsc_bytecode_encode(
    ctx: JSContextRef,
    url: *const std::os::raw::c_char,
    src: *const std::os::raw::c_char,
    is_module: i32,
    out_len: *mut usize,
  ) -> *mut u8;
  fn v82jsc_bytecode_free(p: *mut u8);
  fn v82jsc_program_eval_cached(
    ctx: JSContextRef,
    url: *const std::os::raw::c_char,
    src: *const std::os::raw::c_char,
    bytecode: *const u8,
    bytecode_len: usize,
    exc_out: *mut JSValueRef,
  ) -> JSValueRef;
}

// Script source JSValueRef -> its consumed code-cache bytes, so v8__Script__Run
// can evaluate from bytecode (skip parse+codegen). Populated by CompileModule's
// script/ScriptCompiler consume path.
#[cfg(feature = "vendor_jsc")]
thread_local! {
  static SCRIPT_BYTECODE: std::cell::RefCell<
    std::collections::HashMap<usize, std::vec::Vec<u8>>,
  > = std::cell::RefCell::new(std::collections::HashMap::new());
}

// Startup R&D: total JS bytes + wall-time spent compiling at boot. Sizes the
// bytecode-cache payoff (V82JSC_COMPILE_STATS). Printed by v8__V8__Dispose.
pub(crate) static COMPILE_BYTES: std::sync::atomic::AtomicUsize =
  std::sync::atomic::AtomicUsize::new(0);
pub(crate) static COMPILE_NANOS: std::sync::atomic::AtomicU64 =
  std::sync::atomic::AtomicU64::new(0);

#[inline]
pub(crate) fn compile_stat<T>(src_len: usize, f: impl FnOnce() -> T) -> T {
  if std::env::var_os("V82JSC_COMPILE_STATS").is_none() {
    return f();
  }
  let t = std::time::Instant::now();
  let r = f();
  use std::sync::atomic::Ordering;
  COMPILE_NANOS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
  COMPILE_BYTES.fetch_add(src_len, Ordering::Relaxed);
  r
}

/// Snapshot R&D: append `(specifier, source)` to a dump dir in load order — one
/// `NNN.<sanitized-spec>.js` file per module plus a `MANIFEST` of the order.
/// Used to build/validate the runtime bundler offline. Not on any hot path.
fn dump_module(dir: &std::path::Path, specifier: &str, source: &str) {
  use std::io::Write;
  let _ = std::fs::create_dir_all(dir);
  let manifest = dir.join("MANIFEST");
  let idx = std::fs::read_to_string(&manifest)
    .map(|s| s.lines().count())
    .unwrap_or(0);
  let safe: std::string::String = specifier
    .chars()
    .map(|c| if c.is_alphanumeric() { c } else { '_' })
    .collect();
  let fname = format!("{idx:03}.{safe}.js");
  let _ = std::fs::write(dir.join(&fname), source);
  if let Ok(mut f) = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(&manifest)
  {
    let _ = writeln!(f, "{fname}\t{specifier}");
  }
}

/// Whether the native JSModuleRecord path is active (vendored JSC only).
#[inline]
fn native_modules_enabled() -> bool {
  // Native JSModuleRecords are the module implementation on vendored JSC (the
  // glue needs WebKit's C++ internals). System JSC has no module C++ API, so it
  // falls back to the rewrite_es_module path. No runtime flag.
  cfg!(feature = "vendor_jsc")
}

/// Whether a module with key `specifier` should use the native JSModuleRecord
/// path. Deno's runtime internals (`ext:`, `node:`, `checkin:`) and synthetic
/// modules (JSON, virtual op modules) can't be linked as JSModuleRecords, so
/// only real source-text ESM URLs (file/http/https/npm/jsr/data) qualify.
#[cfg(feature = "vendor_jsc")]
fn native_eligible(_specifier: &str) -> bool {
  // On the vendored build EVERY module is handled by real JSC records — source
  // text via JSModuleRecord, deno's V8-synthetic modules (ops/JSON/node facades)
  // via JSC SyntheticModuleRecords (see CreateSyntheticModule). The
  // rewrite_es_module string rewriter is NEVER used here; it survives only for
  // the system-JavaScriptCore build, which has no C++ module API. So: always
  // native.
  true
}

/// Parse `src` (module key `url`) into a native JSModuleRecord, returning the
/// opaque handle and its requested specifiers. None on parse error (the
/// exception is recorded as pending on `ctx`).
#[cfg(feature = "vendor_jsc")]
unsafe fn native_parse(
  ctx: JSContextRef,
  url: &str,
  src: &str,
) -> Option<(*mut std::ffi::c_void, Vec<std::string::String>)> {
  let url_c = std::ffi::CString::new(url).ok()?;
  let src_c = std::ffi::CString::new(src).ok()?;
  let mut exc: JSValueRef = ptr::null();
  let handle = compile_stat(src.len(), || unsafe {
    v82jsc_module_parse(ctx, url_c.as_ptr(), src_c.as_ptr(), &mut exc)
  });
  if handle.is_null() {
    let e = if exc.is_null() {
      unsafe { make_generic_error(ctx, "module parse failed") }
    } else {
      exc
    };
    unsafe { crate::jsc::core::record_pending_exception(ctx, e) };
    return None;
  }
  let count = unsafe { v82jsc_module_request_count(handle) };
  let mut specs = Vec::with_capacity(count.max(0) as usize);
  for i in 0..count {
    let mut buf = vec![0u8; 1024];
    let n = unsafe {
      v82jsc_module_request_at(
        handle,
        i,
        buf.as_mut_ptr() as *mut std::os::raw::c_char,
        buf.len() as i32,
      )
    };
    if n < 0 {
      continue;
    }
    let n = n as usize;
    if n >= buf.len() {
      // Specifier longer than the buffer; re-fetch with the exact size.
      buf = vec![0u8; n + 1];
      let n2 = unsafe {
        v82jsc_module_request_at(
          handle,
          i,
          buf.as_mut_ptr() as *mut std::os::raw::c_char,
          buf.len() as i32,
        )
      };
      if n2 < 0 {
        continue;
      }
    }
    buf.truncate(n);
    if let Ok(s) = std::string::String::from_utf8(buf) {
      specs.push(s);
    }
  }
  Some((handle, specs))
}

unsafe extern "C" fn mod_finalize(object: JSObjectRef) {
  crate::jsc::core::ffi_guard(
    || {
      let p = unsafe { JSObjectGetPrivate(object) } as *mut SyntheticModule;
      if !p.is_null() {
        let m = unsafe { Box::from_raw(p) };
        if !m.namespace.is_null() && !m.ctx.is_null() {
          unsafe { JSValueUnprotect(m.ctx, m.namespace as JSValueRef) };
        }
        #[cfg(feature = "vendor_jsc")]
        if !m.native.is_null() {
          let rec = unsafe { v82jsc_module_record_ptr(m.native) };
          if !rec.is_null() {
            NATIVE_RECORD_MAP.with(|map| {
              map.borrow_mut().remove(&(rec as usize));
            });
          }
          unsafe { v82jsc_module_release(m.native) };
        }
      }
    },
    || (),
  )
}

thread_local! {
    static MOD_CLASS: std::cell::Cell<JSClassRef> =
        const { std::cell::Cell::new(ptr::null_mut()) };
    // Modules whose body is currently executing. GetStatus reports these as
    // Evaluated (not Evaluating) so a re-entrant require() in a cyclic graph
    // sees the partial namespace instead of deno's cycle error.
    static EVAL_BODY_RUNNING: std::cell::RefCell<Vec<*const Module>> =
        const { std::cell::RefCell::new(Vec::new()) };

    // deno's HostImportModuleDynamicallyCallback. JSC's C-API global object has
    // no module loader, so `import()` throws "No module loader provided"; our
    // WebKit patch wires moduleLoaderImportModule -> v82jsc_dynamic_import,
    // which calls this to load+evaluate the module and returns deno's promise.
    static DYN_IMPORT_CB: std::cell::Cell<
        Option<crate::isolate::RawHostImportModuleDynamicallyCallback>,
    > = const { std::cell::Cell::new(None) };

    // Maps a native JSC record pointer -> deno Module wrapper, so the import.meta
    // hook (moduleLoaderCreateImportMetaProperties) can find the module and route
    // to deno's import_meta_cb (url/main/resolve).
    static NATIVE_RECORD_MAP: std::cell::RefCell<
        std::collections::HashMap<usize, *const Module>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// `import.meta` for a native module. JSC's moduleLoaderCreateImportMetaProperties
/// hook calls this with the raw record pointer + module key. Routes to deno's
/// HostInitializeImportMetaObject callback (sets url/main/resolve); falls back to
/// `{ url: key }` if the module wrapper isn't found.
#[cfg(feature = "vendor_jsc")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_create_import_meta(
  ctx: JSContextRef,
  record_ptr: *mut std::ffi::c_void,
  key: JSStringRef,
) -> JSValueRef {
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    crate::jsc::core::restore_current(current_iso());
    let module = NATIVE_RECORD_MAP
      .with(|m| m.borrow().get(&(record_ptr as usize)).copied());
    let context = ctx as *const Context;
    if let Some(module) = module {
      let meta = build_import_meta(context, module);
      if !meta.is_null() {
        return meta;
      }
    }
    // Fallback: a plain object carrying just the URL (the module key).
    let meta = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
    if !key.is_null() {
      let url_key = JSStringCreateWithUTF8CString(c"url".as_ptr());
      let url_val = JSValueMakeString(ctx, key);
      JSObjectSetProperty(ctx, meta, url_key, url_val, 0, ptr::null_mut());
      JSStringRelease(url_key);
    }
    meta as JSValueRef
  }
}

pub(crate) fn set_dynamic_import_callback(
  cb: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
  DYN_IMPORT_CB.with(|c| c.set(Some(cb)));
}

/// Called from the WebKit patch (JSAPIGlobalObject::moduleLoaderImportModule)
/// when JS runs `import(specifier)`. Routes to deno's dynamic-import callback
/// and returns the promise (resolves to the module namespace). Returns null on
/// no callback; the patch then rejects the import promise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_dynamic_import(
  ctx: JSContextRef,
  specifier: JSStringRef,
  referrer: JSStringRef,
  attr_type: i32,
) -> JSValueRef {
  let Some(cb) = DYN_IMPORT_CB.with(|c| c.get()) else {
    return ptr::null();
  };
  if ctx.is_null() || specifier.is_null() {
    return ptr::null();
  }
  unsafe {
    crate::jsc::core::restore_current(current_iso());
    let spec_val = JSValueMakeString(ctx, specifier);
    let ref_val = if referrer.is_null() {
      JSValueMakeUndefined(ctx)
    } else {
      JSValueMakeString(ctx, referrer)
    };
    // deno reads dynamic-import attributes as [key, value] pairs and derives the
    // requested module type — carry the `with { type: ... }` (json/wasm).
    let type_str = match attr_type {
      3 => Some("json"),
      2 => Some("webassembly"),
      _ => None,
    };
    let empty_attrs = if let Some(ts) = type_str {
      let mk = |s: &str| -> JSValueRef {
        let c = std::ffi::CString::new(s).unwrap();
        let js = JSStringCreateWithUTF8CString(c.as_ptr());
        let v = JSValueMakeString(ctx, js);
        JSStringRelease(js);
        v
      };
      let pair = [mk("type"), mk(ts)];
      JSObjectMakeArray(ctx, 2, pair.as_ptr(), ptr::null_mut()) as JSValueRef
    } else {
      JSObjectMakeArray(ctx, 0, ptr::null(), ptr::null_mut()) as JSValueRef
    };
    // deno reads host_defined_options as a PrimitiveArray and calls .length()
    // (read_host_defined_options_kind); passing undefined makes it transmute a
    // non-array and crash. V8 supplies an EMPTY PrimitiveArray when the host set
    // none — mirror that with an empty JS array.
    let empty_opts =
      JSObjectMakeArray(ctx, 0, ptr::null(), ptr::null_mut()) as JSValueRef;

    let context = ctx as *const Context;
    let host_opts = intern_ctx::<Data>(ctx, empty_opts);
    let ref_h = intern_ctx::<crate::Value>(ctx, ref_val);
    let spec_h = intern_ctx::<V8String>(ctx, spec_val);
    let attr_h = intern_ctx::<FixedArray>(ctx, empty_attrs);

    let (Some(c_l), Some(h_l), Some(r_l), Some(s_l), Some(a_l)) = (
      crate::Local::from_raw(context),
      crate::Local::from_raw(host_opts),
      crate::Local::from_raw(ref_h),
      crate::Local::from_raw(spec_h),
      crate::Local::from_raw(attr_h),
    ) else {
      return ptr::null();
    };

    // JS-stack-exhaustion guard. deno's host dynamic-import callback
    // (bindings.rs `host_import_module_dynamically`) does
    // `v8::PromiseResolver::new(scope).unwrap()` — i.e. our
    // `JSObjectMakeDeferredPromise`. A runaway recursion that reaches `import()`
    // — deno_core's `dyn_import_recurse_err`: `function bar(v){ bar(import(0)); }
    // bar("foo")` — drives JSC to its *JS* stack limit, at which point that
    // allocation returns null and deno's `.unwrap()` panics. The panic unwinds
    // across rusty_v8's `extern "C"` dynamic-import abi_adapter, which aborts the
    // process ("panic in a function that cannot unwind", SIGABRT) — uncatchable
    // by our `ffi_guard`, so it truncates the whole test binary. The
    // native-C-stack guard in `dynamic_import_js_cb` can't catch this: JSC hits
    // its JS recursion limit while the native stack still has >100 KiB free.
    // Probe the exact operation that fails — try to make a deferred promise on
    // this context. JSC gates promise allocation on the native stack pointer, so
    // we must probe with at least as much native stack already consumed as deno
    // burns between here and its own `PromiseResolver::new` (the `extern "C"`
    // abi_adapter, the callback scope, and the callback body's locals); see
    // [`can_alloc_promise_with_margin`]. If the probe fails the JS stack is
    // exhausted, so refuse to enter deno's callback and return null; the runaway
    // recursion still throws the RangeError the test expects, and no abort
    // occurs.
    if !can_alloc_promise_with_margin(ctx) {
      return ptr::null();
    }

    let promise = cb(c_l, h_l, r_l, s_l, a_l);
    promise as JSValueRef
  }
}

/// Native-stack margin (bytes) reserved before probing promise allocation, to
/// cover what deno's host dynamic-import callback burns between
/// `v82jsc_dynamic_import` calling into it and its own
/// `v8::PromiseResolver::new(scope).unwrap()`: the `extern "C"` abi_adapter, the
/// `CallbackScope`, and the callback body's locals/strings. JSC fails promise
/// allocation as a function of the native stack pointer, so probing at the
/// current depth succeeds while deno's deeper allocation still null-faults and
/// aborts. Probing with this much stack already consumed makes the probe
/// predict deno's failure. Comfortably exceeds deno's actual frame depth (a few
/// KiB); over-reserving only bails a doomed runaway recursion an iteration
/// sooner (it still throws its RangeError) and never trips a legitimate
/// `import()`, which runs with megabytes of headroom.
const DYN_IMPORT_FRAME_MARGIN: usize = 64 * 1024;

/// True if JSC can allocate a deferred promise (what deno's dynamic-import
/// callback does via `v8::PromiseResolver::new`) with [`DYN_IMPORT_FRAME_MARGIN`]
/// bytes of native stack already consumed — i.e. with margin for deno's own
/// frames. Bails to `false` (don't enter the callback) if there isn't even
/// enough headroom to safely reserve the margin, so the probe can never itself
/// overflow the native stack. Marked `#[inline(never)]` so the reserved buffer
/// is a real stack frame the probe runs beneath.
#[inline(never)]
unsafe fn can_alloc_promise_with_margin(ctx: JSContextRef) -> bool {
  // Don't burn margin we don't have — that would risk a native overflow inside
  // the probe. Treating "too little headroom to probe safely" as "can't alloc"
  // is correct: that only happens deep in a runaway recursion.
  if stack_headroom_bytes() < DYN_IMPORT_FRAME_MARGIN + 16 * 1024 {
    return false;
  }
  let mut reserve = [0u8; DYN_IMPORT_FRAME_MARGIN];
  // Keep the buffer live across the allocation so the probe truly runs beneath
  // it; `black_box` also blocks the optimizer from eliding the reservation.
  std::hint::black_box(reserve.as_mut_ptr());
  let mut resolve: JSObjectRef = ptr::null_mut();
  let mut reject: JSObjectRef = ptr::null_mut();
  let mut exc: JSValueRef = ptr::null();
  let p = unsafe {
    JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exc)
  };
  std::hint::black_box(reserve.as_mut_ptr());
  !p.is_null()
}

/// Remaining native-stack headroom (bytes) on the current thread. Both JSC
/// backends are macOS, so the pthread stack-introspection APIs are available.
/// Returns `usize::MAX` (i.e. "don't guard") if introspection looks wrong, so a
/// surprising platform can never spuriously trip the guard below.
fn stack_headroom_bytes() -> usize {
  unsafe extern "C" {
    fn pthread_self() -> *mut std::ffi::c_void;
    fn pthread_get_stackaddr_np(
      t: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn pthread_get_stacksize_np(t: *mut std::ffi::c_void) -> usize;
  }
  let probe: u8 = 0;
  let sp = &probe as *const u8 as usize;
  unsafe {
    let t = pthread_self();
    if t.is_null() {
      return usize::MAX;
    }
    // macOS: stack grows down, `stackaddr` is the high bound (top of stack).
    let base = pthread_get_stackaddr_np(t) as usize;
    let size = pthread_get_stacksize_np(t);
    let low = base.wrapping_sub(size);
    if base == 0 || size == 0 || sp > base || sp < low {
      return usize::MAX;
    }
    sp - low
  }
}

/// JS-callable bridge `globalThis.__v82jsc_dynamicImport(specifier, referrer)`.
/// The module-source rewriter turns dynamic `import(...)` into a call to this so
/// dynamic import works WITHOUT a native module-loader hook -- crucial for the
/// system JavaScriptCore.framework, whose shipped JSAPIGlobalObject has no
/// `moduleLoaderImportModule` (that hook is our vendored-only C++ patch). Routes
/// to the same deno callback as `v82jsc_dynamic_import`.
unsafe extern "C" fn dynamic_import_js_cb(
  ctx: JSContextRef,
  _function: JSObjectRef,
  _this: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  exception: *mut JSValueRef,
) -> JSValueRef {
  crate::jsc::core::ffi_guard(
    || unsafe {
      // Native-stack guard. A runaway-recursion script — e.g. deno_core's
      // `dyn_import_recurse_err`: `function bar(){ bar(import(0)); } bar()` —
      // recurses in JS until just under JSC's stack limit, then calls this
      // bridge on the deepest frame. The host dynamic-import callback needs a
      // non-trivial amount of native stack; entering it with almost none left
      // overflows the C stack (a SIGSEGV that even ffi_guard can't unwind)
      // instead of surfacing the RangeError the runaway recursion is supposed
      // to throw. Bail out with a throwable error while there is still stack to
      // unwind through — this both avoids the crash AND gives the test the
      // RangeError it expects. The host callback only needs a handful of native
      // frames (a few KiB), so 64 KiB is ample headroom for it yet small enough
      // that a legitimate `import()` reached through a deep-but-normal frame
      // chain — e.g. on a 512 KiB worker thread that has already consumed some
      // stack — still passes; only genuine runaway recursion approaching the
      // guard page trips it.
      const MIN_STACK_HEADROOM: usize = 64 * 1024;
      if stack_headroom_bytes() < MIN_STACK_HEADROOM {
        if !exception.is_null() {
          *exception =
            make_generic_error(ctx, "Maximum call stack size exceeded");
        }
        return JSValueMakeUndefined(ctx);
      }
      if argc < 1 || argv.is_null() {
        return JSValueMakeUndefined(ctx);
      }
      let mut exc: JSValueRef = ptr::null();
      let spec = JSValueToStringCopy(ctx, *argv, &mut exc);
      if spec.is_null() {
        return JSValueMakeUndefined(ctx);
      }
      let referrer = if argc >= 2 && JSValueIsString(ctx, *argv.add(1)) {
        JSValueToStringCopy(ctx, *argv.add(1), &mut exc)
      } else {
        ptr::null_mut()
      };
      let promise = v82jsc_dynamic_import(ctx, spec, referrer, 0);
      JSStringRelease(spec);
      if !referrer.is_null() {
        JSStringRelease(referrer);
      }
      if promise.is_null() {
        JSValueMakeUndefined(ctx)
      } else {
        promise
      }
    },
    || unsafe { JSValueMakeUndefined(ctx) },
  )
}

/// Install `globalThis.__v82jsc_dynamicImport` on a context. Called for every
/// context the embedder creates (see core.rs).
pub(crate) unsafe fn install_dynamic_import_global(gctx: JSGlobalContextRef) {
  unsafe {
    let name =
      JSStringCreateWithUTF8CString(c"__v82jsc_dynamicImport".as_ptr());
    let f =
      JSObjectMakeFunctionWithCallback(gctx, name, Some(dynamic_import_js_cb));
    if f.is_null() {
      JSStringRelease(name);
      return;
    }
    let global = JSContextGetGlobalObject(gctx);
    // kJSPropertyAttributeDontEnum = 2
    JSObjectSetProperty(
      gctx,
      global,
      name,
      f as JSValueRef,
      2,
      ptr::null_mut(),
    );
    JSStringRelease(name);
  }
}

/// Rewrite dynamic `import(` call sites to `__v82jsc_dynImport(` (a per-module
/// closure injected by the caller that forwards to the host bridge with the
/// module's URL as referrer). Matches the `import` keyword immediately followed
/// by optional whitespace and `(`, only when not preceded by an identifier char
/// (so `Reimport(` is left alone). `import.meta` / `import x from` have no `(`
/// next, so they never match.
pub(crate) fn rewrite_dynamic_import_calls(body: &str) -> std::string::String {
  let bytes = body.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(body.len());
  let mut i = 0;
  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
  while i < bytes.len() {
    if bytes[i] == b'i'
      && body[i..].starts_with("import")
      && (i == 0 || !is_ident(bytes[i - 1]))
    {
      // skip whitespace after `import`
      let mut j = i + 6;
      while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
      }
      if j < bytes.len() && bytes[j] == b'(' {
        out.extend_from_slice(b"__v82jsc_dynImport(");
        i = j + 1;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }
  std::string::String::from_utf8(out).unwrap_or_else(|_| body.to_string())
}

fn mod_class() -> JSClassRef {
  MOD_CLASS.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let def = ModJSClassDefinition {
      version: 0,
      attributes: 0,
      className: c"v8jsc_module".as_ptr(),
      parentClass: ptr::null_mut(),
      staticValues: ptr::null(),
      staticFunctions: ptr::null(),
      initialize: ptr::null(),
      finalize: mod_finalize as *const std::os::raw::c_void,
      hasProperty: ptr::null(),
      getProperty: ptr::null(),
      setProperty: ptr::null(),
      deleteProperty: ptr::null(),
      getPropertyNames: ptr::null(),
      callAsFunction: ptr::null(),
      callAsConstructor: ptr::null(),
      hasInstance: ptr::null(),
      convertToType: ptr::null(),
    };
    let cls =
      unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
    c.set(cls);
    cls
  })
}

#[inline]
fn module_state<'a>(this: *const Module) -> Option<&'a mut SyntheticModule> {
  if this.is_null() {
    return None;
  }
  let obj = jsval(this) as JSObjectRef;
  let p = unsafe { JSObjectGetPrivate(obj) } as *mut SyntheticModule;
  if p.is_null() {
    None
  } else {
    Some(unsafe { &mut *p })
  }
}

unsafe fn eval_value_as_script(
  ctx: JSContextRef,
  source_val: JSValueRef,
  source_url: Option<&str>,
) -> JSValueRef {
  if ctx.is_null() || source_val.is_null() {
    return ptr::null();
  }
  let mut exc: JSValueRef = ptr::null();

  // System JavaScriptCore has NO `moduleLoaderImportModule` hook — that is the
  // vendored-only WebKit C++ patch — so a plain-script `import()` evaluated via
  // `JSEvaluateScript` can't reach the dynamic-import bridge through `sourceURL`
  // at all. Do here what the module/CJS eval paths already do for system JSC:
  // rewrite `import(` → `__v82jsc_dynImport(` and inject a per-script closure
  // forwarding to `globalThis.__v82jsc_dynamicImport` (installed on every
  // context) with the ScriptOrigin resource name as the referrer. The vendored
  // backend keeps its native `sourceURL`-based hook untouched (gated below).
  //
  // Only rewrite when the source actually contains a dynamic import (the rewrite
  // changed something) so the common no-`import()` script is byte-for-byte
  // unchanged — no injected prelude, no line/column shift in stack traces. The
  // prelude carries no trailing newline so the body's line N stays line N, and
  // uses `var` (not `const`) so re-evaluating another `import()` script in the
  // same global context can't trip a lexical redeclaration error.
  #[cfg(not(feature = "vendor_jsc"))]
  {
    let src = unsafe { jsstring_to_rust(ctx, source_val) };
    let rewritten = rewrite_dynamic_import_calls(&src);
    if rewritten != src {
      let referrer = source_url.unwrap_or("");
      // `{:?}` emits the referrer as a Rust `Debug` string — double-quoted with
      // backslash escapes — which coincides with JS string-literal syntax, so it
      // safely embeds a referrer (always a resource URL here) without manual
      // escaping. The `CString::new` below rejects any embedded NUL, so a NUL in
      // the referrer fails the rewrite cleanly rather than producing bad JS.
      let prelude = format!(
        "var __v82jsc_dynImport=(s,o)=>globalThis.__v82jsc_dynamicImport(s,{:?},o);",
        referrer
      );
      let wrapped = format!("{prelude}{rewritten}");
      if let Ok(cstr) = std::ffi::CString::new(wrapped) {
        let js_src = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr()) };
        let src_url_cstr =
          source_url.and_then(|u| std::ffi::CString::new(u).ok());
        let src_url_js = src_url_cstr
          .as_ref()
          .map(|c| unsafe { JSStringCreateWithUTF8CString(c.as_ptr()) })
          .unwrap_or(ptr::null_mut());
        let result = unsafe {
          JSEvaluateScript(
            ctx,
            js_src,
            ptr::null_mut(),
            src_url_js,
            1,
            &mut exc,
          )
        };
        unsafe { JSStringRelease(js_src) };
        if !src_url_js.is_null() {
          unsafe { JSStringRelease(src_url_js) };
        }
        if result.is_null() && !exc.is_null() {
          unsafe { crate::jsc::core::record_pending_exception(ctx, exc) };
        }
        return result;
      }
    }
  }

  let src_str = unsafe { JSValueToStringCopy(ctx, source_val, &mut exc) };
  if src_str.is_null() {
    return ptr::null();
  }
  // Thread the ScriptOrigin resource name into JSEvaluateScript's `sourceURL`.
  // JSC records it as the script's SourceOrigin; the dynamic-import hook
  // (moduleLoaderImportModule) reports `sourceOrigin.string()` as the referrer
  // for any `import()` evaluated in this script. Without it deno's loader sees
  // referrer `<eval>`/empty and can't URL-resolve a relative/root-relative
  // dynamic-import specifier (`./b.js`, `/foo.js`). Null `sourceURL` for scripts
  // compiled without a ScriptOrigin.
  let src_url_cstr = source_url.and_then(|u| std::ffi::CString::new(u).ok());
  let src_url_js = src_url_cstr
    .as_ref()
    .map(|c| unsafe { JSStringCreateWithUTF8CString(c.as_ptr()) })
    .unwrap_or(ptr::null_mut());
  let result = unsafe {
    JSEvaluateScript(ctx, src_str, ptr::null_mut(), src_url_js, 1, &mut exc)
  };
  if !src_url_js.is_null() {
    unsafe { JSStringRelease(src_url_js) };
  }
  unsafe { JSStringRelease(src_str) };
  if result.is_null() {
    let iso = crate::jsc::core::current_iso();
    if !iso.is_null() && crate::jsc::terminate::is_terminating(iso) {
      // The watchdog aborted this script. Replace JSC's own termination
      // exception with an empty (undefined) one: deno pairs that with
      // `is_execution_terminating()` to build "execution terminated".
      let undef = unsafe { JSValueMakeUndefined(ctx) };
      unsafe { crate::jsc::core::record_pending_exception(ctx, undef) };
    } else if !exc.is_null() {
      // Surface the eval error so deno's TryCatch (execute_script) sees
      // has_caught() instead of asserting on a null return with no exception.
      unsafe { crate::jsc::core::record_pending_exception(ctx, exc) };
    }
  }
  result
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
  let ctx = current_ctx();
  let v = jsval(this);
  if ctx.is_null() || v.is_null() {
    return 0;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, v, &mut exc);
    if obj.is_null() {
      return 0;
    }
    let name = JSStringCreateWithUTF8CString(c"length".as_ptr());
    let len_val = JSObjectGetProperty(ctx, obj, name, &mut exc);
    JSStringRelease(name);
    if len_val.is_null() {
      return 0;
    }
    JSValueToNumber(ctx, len_val, &mut exc) as int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(
  this: *const FixedArray,
  index: int,
) -> *const Data {
  let ctx = current_ctx();
  let v = jsval(this);
  if ctx.is_null() || v.is_null() || index < 0 {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, v, &mut exc);
    if obj.is_null() {
      return ptr::null();
    }
    let elem = JSObjectGetPropertyAtIndex(ctx, obj, index as u32, &mut exc);
    intern_ctx::<Data>(ctx, elem)
  }
}

thread_local! {
  // Resource name (script URL) per compiled `Script` handle, so `Run` can pass
  // it to `JSEvaluateScript` as the `sourceURL`. JSC reports that as the script's
  // SourceOrigin, which the dynamic-import hook surfaces as the referrer for any
  // `import()` evaluated in the script — without it deno's loader can't resolve a
  // relative/root-relative dynamic-import specifier (see `eval_value_as_script`).
  // Keyed on the source JSValueRef (== the `Script` handle, since `intern_ctx`
  // returns the value pointer unchanged), mirroring SCRIPT_BYTECODE.
  static SCRIPT_RESOURCE_NAMES: std::cell::RefCell<
    std::collections::HashMap<usize, std::string::String>,
  > = std::cell::RefCell::new(std::collections::HashMap::new());
}

// Pull the resource-name string out of a `ScriptOrigin` (slot 0 holds the
// `resource_name` Value pointer, per `v8__ScriptOrigin__CONSTRUCT`). Returns
// None for a null/undefined/empty name.
unsafe fn origin_resource_name(
  ctx: JSContextRef,
  origin: *const ScriptOrigin,
) -> Option<std::string::String> {
  if origin.is_null() || ctx.is_null() {
    return None;
  }
  let name_val = unsafe { *(origin as *const usize) } as JSValueRef;
  if name_val.is_null() {
    return None;
  }
  unsafe {
    if JSValueIsUndefined(ctx, name_val) || JSValueIsNull(ctx, name_val) {
      return None;
    }
    let s = jsstring_to_rust(ctx, name_val);
    if s.is_empty() { None } else { Some(s) }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
  context: *const Context,
  source: *const V8String,
  origin: *const ScriptOrigin,
) -> *const Script {
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = jsval(source);
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  let name = unsafe { origin_resource_name(ctx, origin) };
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    // Pass the script's resource name as the sourceURL so a SyntaxError
    // carries `.sourceURL`/`.line`/`.column` — deno's `from_v8_message` frame
    // fallback bails without a resource name, and `tc_scope.exception()`
    // requires the recorded exception (see below).
    let source_url = name
      .as_deref()
      .and_then(|n| std::ffi::CString::new(n).ok())
      .map(|c| JSStringCreateWithUTF8CString(c.as_ptr()))
      .unwrap_or(ptr::null_mut());
    let ok = JSCheckScriptSyntax(ctx, src_str, source_url, 1, &mut exc);
    JSStringRelease(src_str);
    if !source_url.is_null() {
      JSStringRelease(source_url);
    }
    if !ok {
      // deno reads the compile error via `tc_scope.exception().unwrap()`;
      // record the SyntaxError as pending so that returns the real error
      // (with location) instead of panicking on a missing exception.
      if !exc.is_null() {
        crate::jsc::core::record_pending_exception(ctx, exc);
      }
      return ptr::null();
    }
  }
  if let Some(name) = name {
    SCRIPT_RESOURCE_NAMES
      .with(|m| m.borrow_mut().insert(src_val as usize, name));
  }
  intern_ctx::<Script>(ctx, src_val)
}

thread_local! {
  // Script id per compiled script, assigned on first request. A `Script` and
  // its `UnboundScript` are the same source value here, so they share one.
  static SCRIPT_IDS: std::cell::RefCell<std::collections::HashMap<usize, int>> =
    std::cell::RefCell::new(std::collections::HashMap::new());
}

static NEXT_SCRIPT_ID: std::sync::atomic::AtomicI32 =
  std::sync::atomic::AtomicI32::new(1);

/// JSC does not expose its `SourceID`, so ids are this backend's own: stable
/// and positive for a given script, distinct between scripts.
fn script_id_of(script: JSValueRef) -> int {
  if script.is_null() {
    return 0;
  }
  SCRIPT_IDS.with(|m| {
    *m.borrow_mut().entry(script as usize).or_insert_with(|| {
      NEXT_SCRIPT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    })
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__ScriptId(script: *const Script) -> int {
  script_id_of(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__ScriptId(
  unbound_script: *const UnboundScript,
) -> int {
  script_id_of(jsval(unbound_script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(
  script: *const Script,
) -> *const UnboundScript {
  intern::<UnboundScript>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
  script: *const Script,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = jsval(script);

  // Resource name (script URL) from the compile-time ScriptOrigin, used as the
  // eval `sourceURL` so `import()` in this script resolves relative to it.
  let source_url = SCRIPT_RESOURCE_NAMES
    .with(|m| m.borrow().get(&(src_val as usize)).cloned());

  // Bytecode-cached path: if Compile stashed code cache for this source, eval
  // from bytecode (skip parse+codegen). JSC silently falls back to parsing on a
  // stale/invalid buffer, so this stays correct.
  #[cfg(feature = "vendor_jsc")]
  {
    let bytes =
      SCRIPT_BYTECODE.with(|m| m.borrow().get(&(src_val as usize)).cloned());
    if let Some(bytes) = bytes {
      let result = unsafe {
        let src = jsstring_to_rust(ctx, src_val);
        let src_c = std::ffi::CString::new(src).ok();
        let url_c =
          std::ffi::CString::new(source_url.as_deref().unwrap_or("script"))
            .ok();
        match (src_c, url_c) {
          (Some(s), Some(u)) => {
            let mut exc: JSValueRef = ptr::null();
            let r = compile_stat(bytes.len(), || {
              v82jsc_program_eval_cached(
                ctx,
                u.as_ptr(),
                s.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                &mut exc,
              )
            });
            if r.is_null() {
              let iso = crate::jsc::core::current_iso();
              if !iso.is_null() && crate::jsc::terminate::is_terminating(iso) {
                let undef = JSValueMakeUndefined(ctx);
                crate::jsc::core::record_pending_exception(ctx, undef);
              } else if !exc.is_null() {
                crate::jsc::core::record_pending_exception(ctx, exc);
              }
            }
            r
          }
          _ => ptr::null(),
        }
      };
      if result.is_null() {
        return ptr::null();
      }
      return intern_ctx::<Value>(ctx, result);
    }
  }

  let result =
    unsafe { eval_value_as_script(ctx, src_val, source_url.as_deref()) };
  if result.is_null() {
    return ptr::null();
  }
  intern_ctx::<Value>(ctx, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
  buf: *mut MaybeUninit<ScriptOrigin>,
  resource_name: *const Value,
  _resource_line_offset: i32,
  _resource_column_offset: i32,
  _resource_is_shared_cross_origin: bool,
  _script_id: i32,
  _source_map_url: *const Value,
  _resource_is_opaque: bool,
  _is_wasm: bool,
  _is_module: bool,
  _host_defined_options: *const Data,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
      *(buf as *mut usize) = resource_name as usize;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
  buf: *mut MaybeUninit<Source>,
  source_string: *const V8String,
  origin: *const ScriptOrigin,
  cached_data: *mut CachedData,
) {
  if buf.is_null() {
    return;
  }
  unsafe {
    ptr::write_bytes(buf as *mut u8, 0u8, size_of::<Source>());
    let slots = buf as *mut usize;
    *slots = source_string as usize;
    if !origin.is_null() {
      *slots.add(1) = *(origin as *const usize);
    }
    // Slot 2: the code cache deno supplies (ConsumeCodeCache); read by Compile.
    *slots.add(2) = cached_data as usize;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__DESTRUCT(_this: *mut Source) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__GetCachedData<'a>(
  _this: *const Source,
) -> *const CachedData<'a> {
  ptr::null()
}

#[inline]
unsafe fn source_string_of(source: *mut Source) -> JSValueRef {
  if source.is_null() {
    return ptr::null();
  }
  unsafe { *(source as *const usize) as JSValueRef }
}

unsafe fn jsstring_to_rust(
  ctx: JSContextRef,
  v: JSValueRef,
) -> std::string::String {
  if ctx.is_null() || v.is_null() {
    return std::string::String::new();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, v, &mut exc);
    if s.is_null() {
      return std::string::String::new();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return std::string::String::new();
    }
    buf.truncate(n - 1);
    std::string::String::from_utf8(buf).unwrap_or_default()
  }
}

unsafe fn resource_name_of(
  ctx: JSContextRef,
  source: *mut Source,
) -> std::string::String {
  if source.is_null() || ctx.is_null() {
    return std::string::String::new();
  }
  let name_val = unsafe { *((source as *const usize).add(1)) } as JSValueRef;
  if name_val.is_null() {
    return std::string::String::new();
  }
  unsafe {
    if JSValueIsUndefined(ctx, name_val) || JSValueIsNull(ctx, name_val) {
      return std::string::String::new();
    }
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, name_val, &mut exc);
    if s.is_null() {
      return std::string::String::new();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return std::string::String::new();
    }
    buf.truncate(n - 1);
    std::string::String::from_utf8(buf).unwrap_or_default()
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
  data: *const u8,
  length: i32,
) -> *mut CachedData<'a> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  let boxed = Box::new(RawCachedData {
    data,
    length,
    rejected: false,
    buffer_policy: 0,
  });
  Box::into_raw(boxed) as *mut CachedData<'a>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE<'a>(
  this: *mut CachedData<'a>,
) {
  if this.is_null() {
    return;
  }
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  unsafe {
    let raw = Box::from_raw(this as *mut RawCachedData);

    if raw.buffer_policy == 1 && !raw.data.is_null() && raw.length > 0 {
      let slice = std::slice::from_raw_parts_mut(
        raw.data as *mut u8,
        raw.length as usize,
      );
      drop(Box::from_raw(slice as *mut [u8]));
    }
    drop(raw);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Compile(
  context: *const Context,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Script {
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
    JSStringRelease(src_str);
    if !ok {
      return ptr::null();
    }
  }
  // Carry the Source's resource name (ScriptOrigin) so Run uses it as the eval
  // `sourceURL` — same dynamic-import referrer fix as v8__Script__Compile.
  let name = unsafe { resource_name_of(ctx, source) };
  if !name.is_empty() {
    SCRIPT_RESOURCE_NAMES
      .with(|m| m.borrow_mut().insert(src_val as usize, name));
  }
  // ConsumeCodeCache: deno passed precompiled bytecode. Stash it so Run can
  // evaluate from bytecode (skip parse+codegen) and leave the CachedData
  // un-rejected so deno keeps reusing it.
  #[cfg(feature = "vendor_jsc")]
  unsafe {
    if let Some(bytes) = cached_data_bytes(source) {
      if !bytes.is_empty() {
        SCRIPT_BYTECODE
          .with(|m| m.borrow_mut().insert(src_val as usize, bytes.to_vec()));
      }
    }
  }
  intern_ctx::<Script>(ctx, src_val)
}

/// Read the code-cache bytes deno supplied via Source slot 2 (a RawCachedData),
/// and mark it not-rejected so deno reuses it.
#[cfg(feature = "vendor_jsc")]
unsafe fn cached_data_bytes<'a>(source: *mut Source) -> Option<&'a [u8]> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  if source.is_null() {
    return None;
  }
  let cd = unsafe { *((source as *const usize).add(2)) } as *mut RawCachedData;
  if cd.is_null() {
    return None;
  }
  unsafe {
    let raw = &mut *cd;
    raw.rejected = false;
    if raw.data.is_null() || raw.length <= 0 {
      return None;
    }
    let bytes = std::slice::from_raw_parts(raw.data, raw.length as usize);
    // Bytes this backend did not frame (foreign data, or a cache written
    // before the framing existed) are rejected so deno regenerates them.
    match code_cache_payload(bytes) {
      Ok(payload) => Some(payload),
      Err(_) => {
        raw.rejected = true;
        None
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Module {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };

  let text = unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, src_val, &mut exc);
    if s.is_null() {
      return ptr::null();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return ptr::null();
    }
    buf.truncate(n - 1);
    match std::string::String::from_utf8(buf) {
      Ok(t) => t,
      Err(_) => return ptr::null(),
    }
  };

  let specifier = unsafe { resource_name_of(ctx, source) };

  // Snapshot R&D: dump every compiled module (in load order) to a dir so the
  // runtime-bundler can be built/validated against real boot data. Gated.
  if let Some(dir) = std::env::var_os("V82JSC_DUMP_MODULES") {
    dump_module(std::path::Path::new(&dir), &specifier, &text);
  }

  // VENDORED: every module is a real JSC record (source via JSModuleRecord,
  // deno's V8-synthetic modules via SyntheticModuleRecord). No string rewriter.
  #[cfg(feature = "vendor_jsc")]
  {
    let _ = native_modules_enabled;
    return match unsafe { native_parse(ctx, &specifier, &text) } {
      Some((handle, specs)) => {
        let is_async = has_top_level_await(&text);
        let state = Box::new(SyntheticModule {
          ctx: gctx,
          status: ModuleStatus::Uninstantiated,
          export_names: Vec::new(),
          eval_steps: None,
          source: None,
          source_map_url: extract_source_mapping_url(&text),
          import_offsets: compute_import_offsets(&text, &specs),
          import_attributes: compute_import_attributes(&text, &specs),
          import_specifiers: specs,
          namespace: ptr::null_mut(),
          specifier,
          dependencies: Vec::new(),
          is_async,
          native: handle,
        });
        let obj = unsafe {
          JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _)
        };
        let module = intern_ctx::<Module>(ctx, obj as JSValueRef);
        // Register record -> module so the import.meta hook finds this wrapper.
        let rec = unsafe { v82jsc_module_record_ptr(handle) };
        if !rec.is_null() {
          NATIVE_RECORD_MAP
            .with(|m| m.borrow_mut().insert(rec as usize, module));
        }
        module
      }
      None => ptr::null(), // native_parse recorded the exception
    };
  }

  // SYSTEM JavaScriptCore.framework: no C++ module API — fall back to the string
  // rewriter. Targets BUNDLED apps (deno compile / desktop), where the user
  // graph is flattened so the rewriter's unbundled-ESM limits never bite.
  #[cfg(not(feature = "vendor_jsc"))]
  {
    let Some(rewrite) = rewrite_es_module(&text) else {
      // The rewriter couldn't handle this module's syntax. Record a real
      // SyntaxError so deno reports it instead of unwrapping None and panicking
      // (libs/core/modules/map.rs `maybe_module.unwrap()`).
      unsafe {
        let msg = format!(
          "v82jsc: unsupported ES module syntax in {}",
          if specifier.is_empty() {
            "<module>"
          } else {
            &specifier
          }
        );
        let e = make_generic_error(ctx, &msg);
        crate::jsc::core::record_pending_exception(ctx, e);
      }
      return ptr::null();
    };

    let namespace =
      unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

    // Register the namespace into the global registry NOW (at compile), not at
    // Evaluate — a barrel module evaluates last, but its importers read its
    // namespace earlier. Install live re-export getters so `export { X } from
    // spec` resolves through to spec's namespace at access time.
    unsafe {
      register_module_namespace(ctx, &specifier, namespace);
      install_reexport_getters(ctx, namespace, &rewrite.reexports);
    }

    let state = Box::new(SyntheticModule {
      ctx: gctx,
      status: ModuleStatus::Uninstantiated,
      export_names: rewrite.export_names,
      eval_steps: None,
      source: Some(rewrite.body),
      source_map_url: extract_source_mapping_url(&text),
      import_offsets: compute_import_offsets(&text, &rewrite.imports),
      import_attributes: compute_import_attributes(&text, &rewrite.imports),
      import_specifiers: rewrite.imports,
      namespace,
      specifier,
      dependencies: Vec::new(),
      is_async: rewrite.is_async,
      native: ptr::null_mut(),
    });
    let obj =
      unsafe { JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _) };
    intern_ctx::<Module>(ctx, obj as JSValueRef)
  }
}

struct RewrittenModule {
  body: std::string::String,
  export_names: Vec<std::string::String>,
  imports: Vec<std::string::String>,
  is_async: bool,
  // (exported, source spec, source local): `export { local as exported } from
  // spec`. Installed as LIVE getters on the namespace at compile time so a
  // barrel's re-exports resolve through to the source module even before the
  // barrel's own body runs (ESM live re-export bindings).
  reexports: Vec<(
    std::string::String,
    std::string::String,
    std::string::String,
  )>,
}

fn strip_js_comments(src: &str) -> std::string::String {
  let b = src.as_bytes();
  let n = b.len();
  let mut out: Vec<u8> = Vec::with_capacity(n);
  let mut i = 0usize;
  let mut prev_sig = 0u8;
  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
  while i < n {
    let c = b[i];
    match c {
      b'/' if i + 1 < n && b[i + 1] == b'/' => {
        i += 2;
        while i < n && b[i] != b'\n' {
          i += 1;
        }
      }
      b'/' if i + 1 < n && b[i + 1] == b'*' => {
        i += 2;
        while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
          i += 1;
        }
        i += 2;
      }
      b'"' | b'\'' => {
        let q = c;
        out.push(c);
        i += 1;
        while i < n {
          out.push(b[i]);
          if b[i] == b'\\' && i + 1 < n {
            out.push(b[i + 1]);
            i += 2;
            continue;
          }
          if b[i] == q {
            i += 1;
            break;
          }
          i += 1;
        }
        prev_sig = q;
      }
      b'`' => {
        out.push(b'`');
        i += 1;
        while i < n {
          out.push(b[i]);
          if b[i] == b'\\' && i + 1 < n {
            out.push(b[i + 1]);
            i += 2;
            continue;
          }
          if b[i] == b'`' {
            i += 1;
            break;
          }
          i += 1;
        }
        prev_sig = b'`';
      }
      b'/' => {
        let regex_ok = matches!(
          prev_sig,
          0 | b'('
            | b','
            | b'='
            | b':'
            | b'['
            | b'!'
            | b'&'
            | b'|'
            | b'?'
            | b'{'
            | b'}'
            | b';'
            | b'+'
            | b'-'
            | b'*'
            | b'%'
            | b'<'
            | b'>'
            | b'^'
            | b'~'
        );
        if regex_ok {
          out.push(b'/');
          i += 1;
          let mut in_class = false;
          while i < n {
            out.push(b[i]);
            if b[i] == b'\\' && i + 1 < n {
              out.push(b[i + 1]);
              i += 2;
              continue;
            }
            match b[i] {
              b'[' => in_class = true,
              b']' => in_class = false,
              b'/' if !in_class => {
                i += 1;
                break;
              }
              _ => {}
            }
            i += 1;
          }
          prev_sig = b'/';
        } else {
          out.push(b'/');
          prev_sig = b'/';
          i += 1;
        }
      }
      _ => {
        if !c.is_ascii_whitespace() {
          prev_sig = if is_ident(c) { b'a' } else { c };
        }
        out.push(c);
        i += 1;
      }
    }
  }
  std::string::String::from_utf8_lossy(&out).into_owned()
}

fn join_module_statements(src: &str) -> Vec<std::string::String> {
  let lines: Vec<&str> = src.lines().collect();
  let mut result: Vec<std::string::String> = Vec::new();
  let mut i = 0;
  while i < lines.len() {
    let trimmed = lines[i].trim();
    // Only JOIN true named-binding lists (`import {`, `export {`, `import a,
    // {`). A declaration like `export class X {` / `export function f() {` also
    // "starts with export and has an unclosed {", but its `{` opens a body —
    // joining to the next `}` would swallow the declaration into the `export {}`
    // handler and drop the class/function header.
    let after_kw = trimmed
      .strip_prefix("import")
      .or_else(|| trimmed.strip_prefix("export"))
      .map(str::trim_start)
      .unwrap_or("");
    let import_binding = trimmed.starts_with("import")
      && (after_kw.starts_with('{')
        || after_kw
          .split_once(',')
          .map(|(_, r)| r.trim_start().starts_with('{'))
          .unwrap_or(false));
    let export_binding =
      trimmed.starts_with("export") && after_kw.starts_with('{');
    let starts_brace_stmt = (import_binding || export_binding)
      && trimmed.contains('{')
      && !trimmed.contains('}');
    if starts_brace_stmt {
      let mut buf = std::string::String::new();
      buf.push_str(lines[i].trim_end());
      buf.push(' ');
      let mut closed = false;
      i += 1;
      while i < lines.len() {
        let l = lines[i];
        buf.push_str(l.trim());
        buf.push(' ');
        if l.contains('}') {
          closed = true;

          i += 1;

          if !buf.contains(" from ")
            && !buf.trim_end().ends_with(';')
            && i < lines.len()
            && lines[i].trim_start().starts_with("from ")
          {
            buf.push_str(lines[i].trim());
            buf.push(' ');
            i += 1;
          }
          break;
        }
        i += 1;
      }
      let _ = closed;
      result.push(buf.split_whitespace().collect::<Vec<_>>().join(" "));
    } else {
      result.push(lines[i].to_string());
      i += 1;
    }
  }
  result
}

fn strip_leading_block_comments(mut s: &str) -> &str {
  loop {
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix("/*") {
      if let Some(end) = rest.find("*/") {
        s = &rest[end + 2..];
        continue;
      }
    }
    return s;
  }
}

fn rewrite_es_module(src: &str) -> Option<RewrittenModule> {
  let mut out = std::string::String::new();

  let mut imports_out = std::string::String::new();

  let mut exports_out = std::string::String::new();
  let mut export_names: Vec<std::string::String> = Vec::new();
  let mut imports: Vec<std::string::String> = Vec::new();
  let mut reexports: Vec<(
    std::string::String,
    std::string::String,
    std::string::String,
  )> = Vec::new();
  // localName -> (source spec, source export name) for every imported binding,
  // so a later `export { localName }` can re-export it LIVE to the source.
  let mut import_bindings: std::collections::HashMap<
    std::string::String,
    (std::string::String, std::string::String),
  > = std::collections::HashMap::new();
  // `export * from spec` (None) / `export * as name from spec` (Some(name)).
  let mut star_reexports: Vec<(
    std::string::String,
    Option<std::string::String>,
  )> = Vec::new();

  let cleaned = strip_js_comments(src);
  let logical = join_module_statements(&cleaned);

  for raw_line in logical.iter() {
    let raw_line: &str = raw_line.as_str();
    let line = raw_line.trim_start();
    let trimmed = strip_leading_block_comments(line.trim());

    if trimmed.starts_with("export *") {
      // `export * from "spec"` (star re-export) or `export * as ns from "spec"`
      // (namespace re-export). Collect; emit copy code AFTER local exports so
      // local/named exports win (ESM precedence).
      let spec = extract_specifier(trimmed).unwrap_or_default();
      if spec.is_empty() {
        return None;
      }
      imports.push(spec.clone());
      let after = trimmed["export *".len()..].trim_start();
      let as_name = after.strip_prefix("as ").map(|rest| {
        rest.split(" from ").next().unwrap_or("").trim().to_string()
      });
      if let Some(ref n) = as_name {
        if !n.is_empty() {
          export_names.push(n.clone());
        }
      }
      star_reexports.push((spec, as_name));
      continue;
    }

    if trimmed.starts_with("import ") || trimmed == "import" {
      let spec = extract_specifier(trimmed).unwrap_or_default();
      if !spec.is_empty() {
        imports.push(spec.clone());
      }
      let module_expr =
        format!("((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}})", spec);

      let clause =
        if let Some((c, _)) = trimmed["import".len()..].split_once(" from ") {
          c.trim()
        } else {
          ""
        };

      if clause.contains('{') {
        let brace_start = clause.find('{').unwrap();
        let head = clause[..brace_start].trim().trim_end_matches(',').trim();
        if !head.is_empty() {
          imports_out
            .push_str(&format!("const {} = {}.default;\n", head, module_expr));
          import_bindings
            .insert(head.to_string(), (spec.clone(), "default".to_string()));
        }

        let names = between(trimmed, '{', '}').unwrap_or_default();
        let mut destructure: Vec<std::string::String> = Vec::new();
        for part in names.split(',') {
          let part = part.trim();
          if part.is_empty() {
            continue;
          }
          if let Some((l, r)) = part.split_once(" as ") {
            destructure.push(format!("{}: {}", l.trim(), r.trim()));
            import_bindings.insert(
              r.trim().to_string(),
              (spec.clone(), l.trim().to_string()),
            );
          } else {
            destructure.push(part.to_string());
            import_bindings
              .insert(part.to_string(), (spec.clone(), part.to_string()));
          }
        }
        imports_out.push_str(&format!(
          "const {{ {} }} = {};\n",
          destructure.join(", "),
          module_expr
        ));
      } else if clause.starts_with("* as ") {
        let name = clause["* as ".len()..].trim();
        if !name.is_empty() {
          imports_out.push_str(&format!("const {} = {};\n", name, module_expr));
          import_bindings
            .insert(name.to_string(), (spec.clone(), "*".to_string()));
        }
      } else if !clause.is_empty() && trimmed.contains(" from ") {
        imports_out
          .push_str(&format!("const {} = {}.default;\n", clause, module_expr));
        import_bindings
          .insert(clause.to_string(), (spec.clone(), "default".to_string()));
      }

      continue;
    }

    if trimmed.starts_with("export {") || trimmed.starts_with("export{") {
      let inner = between(trimmed, '{', '}').unwrap_or_default();

      let reexport_spec = if trimmed.contains(" from ") {
        let spec = extract_specifier(trimmed).unwrap_or_default();
        if !spec.is_empty() {
          imports.push(spec.clone());
        }
        Some(spec)
      } else {
        None
      };
      for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
          continue;
        }
        let (local, exported) = if let Some((l, r)) = part.split_once(" as ") {
          (l.trim().to_string(), r.trim().to_string())
        } else {
          (part.to_string(), part.to_string())
        };
        export_names.push(exported.clone());
        if let Some(spec) = &reexport_spec {
          // `export { local } from spec`: live re-export to spec.
          reexports.push((exported.clone(), spec.clone(), local.clone()));
        } else if let Some((spec, source)) = import_bindings.get(&local) {
          // `import X from spec; export { X }` (barrel pattern): X is an
          // imported binding, so re-export LIVE to its source module instead of
          // snapshotting at body time (the barrel body runs last, after its
          // importers already read the binding).
          reexports.push((exported.clone(), spec.clone(), source.clone()));
        } else {
          exports_out.push_str(&format!("__ns[{:?}] = {};\n", exported, local));
        }
      }
      continue;
    }

    if trimmed.starts_with("export default ") {
      // The default expression may span multiple lines (`export default {`
      // ...multi-line object/class/function...). Emit a `const` binding inline
      // in `out` so the body stays CONTIGUOUS with its opening, and assign the
      // namespace slot from that binding at the end. (A prior single-line
      // `__ns["default"] = (expr)` cut multi-line objects to `({)`.)
      let expr = &trimmed["export default ".len()..];
      export_names.push("default".to_string());
      out.push_str("const __v8jsc_default = ");
      out.push_str(expr);
      out.push('\n');
      exports_out.push_str("__ns[\"default\"] = __v8jsc_default;\n");
      continue;
    }

    if trimmed.starts_with("export const ")
      || trimmed.starts_with("export let ")
      || trimmed.starts_with("export var ")
      || trimmed.starts_with("export function ")
      || trimmed.starts_with("export async function ")
      || trimmed.starts_with("export class ")
    {
      let rest = trimmed.strip_prefix("export ").unwrap();

      let after_kw = rest
        .trim_start_matches("const ")
        .trim_start_matches("let ")
        .trim_start_matches("var ")
        .trim_start_matches("async function ")
        .trim_start_matches("function ")
        .trim_start_matches("class ");

      if after_kw.trim_start().starts_with('{') {
        out.push_str(rest);
        out.push('\n');
        let inner = between(after_kw, '{', '}').unwrap_or_default();
        for part in inner.split(',') {
          let part = part.trim();
          if part.is_empty() {
            continue;
          }

          let local = part
            .split(':')
            .next_back()
            .unwrap_or(part)
            .split('=')
            .next()
            .unwrap_or(part)
            .trim();
          let name: std::string::String = local
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
          if !name.is_empty() {
            export_names.push(name.clone());
            exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
          }
        }
        continue;
      }
      let name: std::string::String = after_kw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
      out.push_str(rest);
      out.push('\n');
      if !name.is_empty() {
        export_names.push(name.clone());
        exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
      }
      continue;
    }

    out.push_str(raw_line);
    out.push('\n');
  }

  // Emit `export *` copies last so explicit/named exports take precedence.
  for (spec, as_name) in &star_reexports {
    let module_expr =
      format!("((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}})", spec);
    match as_name {
      Some(name) if !name.is_empty() => {
        // export * as name from spec -> bind the whole namespace object.
        exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, module_expr));
      }
      _ => {
        // export * from spec -> live-copy every named export except `default`,
        // not overriding bindings this module already defines.
        exports_out.push_str(&format!(
          "{{const __se={m};for(const __k of Object.keys(__se)){{\
             if(__k!==\"default\"&&!Object.prototype.hasOwnProperty.call(__ns,__k)){{\
               Object.defineProperty(__ns,__k,{{get:((k)=>()=>__se[k])(__k),\
                 enumerable:true,configurable:true}});}}}}}}\n",
          m = module_expr
        ));
      }
    }
  }

  let combined = format!("{}{}{}", imports_out, out, exports_out);

  let body = combined.replace("import.meta", "__v8jsc_meta");

  Some(RewrittenModule {
    body,
    export_names,
    imports,
    is_async: has_top_level_await(src),
    reexports,
  })
}

fn has_top_level_await(src: &str) -> bool {
  let b = src.as_bytes();
  let n = b.len();
  let mut i = 0usize;

  let mut fn_body_depths: Vec<i32> = Vec::new();
  let mut depth: i32 = 0;

  let mut pending_fn_body = false;

  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

  while i < n {
    let c = b[i];
    match c {
      b'/' if i + 1 < n && b[i + 1] == b'/' => {
        i += 2;
        while i < n && b[i] != b'\n' {
          i += 1;
        }
      }

      b'/' if i + 1 < n && b[i + 1] == b'*' => {
        i += 2;
        while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
          i += 1;
        }
        i += 2;
      }

      b'"' | b'\'' => {
        let q = c;
        i += 1;
        while i < n {
          if b[i] == b'\\' {
            i += 2;
            continue;
          }
          if b[i] == q {
            i += 1;
            break;
          }
          i += 1;
        }
      }

      b'`' => {
        i += 1;
        while i < n {
          if b[i] == b'\\' {
            i += 2;
            continue;
          }
          if b[i] == b'`' {
            i += 1;
            break;
          }
          i += 1;
        }
      }
      b'{' => {
        depth += 1;
        if pending_fn_body {
          fn_body_depths.push(depth);
          pending_fn_body = false;
        }
        i += 1;
      }
      b'}' => {
        if let Some(&d) = fn_body_depths.last() {
          if d == depth {
            fn_body_depths.pop();
          }
        }
        depth -= 1;
        i += 1;
      }

      b'=' if i + 1 < n && b[i + 1] == b'>' => {
        pending_fn_body = true;
        i += 2;
      }
      _ if is_ident(c) => {
        let start = i;
        while i < n && is_ident(b[i]) {
          i += 1;
        }
        let word = &src[start..i];
        let prev_is_dot = start > 0 && {
          let mut j = start;
          while j > 0 && (b[j - 1] as char).is_whitespace() {
            j -= 1;
          }
          j > 0 && b[j - 1] == b'.'
        };
        match word {
          "function" => {
            pending_fn_body = true;
          }
          "await" if !prev_is_dot => {
            // Genuine top-level await is at brace-depth 0 (a module-top
            // statement). The function-body tracker misses async METHODS
            // (`async foo() {}`, `async *g() {}`, `for await` inside them) —
            // they have no `function` keyword/`=>`, so their await would
            // false-positive as top-level and wrongly mark the module async,
            // deadlocking its importers. Requiring depth 0 avoids that. (Misses
            // the rare TLA inside a top-level `if`/`for` block; acceptable.)
            if depth == 0 && fn_body_depths.is_empty() {
              return true;
            }
          }
          _ => {}
        }
      }
      _ => {
        i += 1;
      }
    }
  }
  false
}

fn extract_specifier(line: &str) -> Option<std::string::String> {
  let scan = match line.rfind(" from ") {
    Some(p) => &line[p + 6..],
    None => line,
  };
  let bytes = scan.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(scan[open + 1..i].to_string());
    }
  }
  None
}

fn extract_attr_type(stmt: &str) -> Option<std::string::String> {
  let kw = stmt
    .find(" with ")
    .or_else(|| stmt.find(" with{"))
    .or_else(|| stmt.find(" assert "))
    .or_else(|| stmt.find(" assert{"))?;
  let clause = &stmt[kw..];
  let tpos = clause.find("type")?;
  let after = &clause[tpos + 4..];
  let bytes = after.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(after[open + 1..i].to_string());
    }
  }
  None
}

fn between(s: &str, open: char, close: char) -> Option<std::string::String> {
  let a = s.find(open)?;
  let b = s[a + 1..].find(close)? + a + 1;
  Some(s[a + 1..b].to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileFunction(
  context: *const Context,
  source: *mut Source,
  arguments_count: usize,
  arguments: *const *const V8String,
  _context_extensions_count: usize,
  _context_extensions: *const *const Object,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Function {
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();

    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let max = JSStringGetMaximumUTF8CStringSize(src_str);
    let mut body = vec![0u8; max];
    JSStringGetUTF8CString(src_str, body.as_mut_ptr() as *mut _, max);
    JSStringRelease(src_str);
    let mut body_str = std::ffi::CStr::from_ptr(body.as_ptr() as *const _)
      .to_string_lossy()
      .into_owned();

    // V8's CompileFunction tolerates a leading `#!` shebang (Node CJS bins
    // start with `#!/usr/bin/env node`); JSEvaluateScript reports it as
    // "Invalid character: '#'". Strip a leading shebang line so the wrapped
    // CJS body compiles instead of falling back to ESM evaluation.
    if body_str.starts_with("#!") {
      let rest = match body_str.find('\n') {
        Some(nl) => body_str[nl + 1..].to_string(),
        None => std::string::String::new(),
      };
      body_str = rest;
    }

    let mut arg_names: Vec<std::string::String> = Vec::new();
    if !arguments.is_null() {
      for i in 0..arguments_count {
        let a = *arguments.add(i);
        let av = jsval(a);
        if av.is_null() {
          continue;
        }
        let s = JSValueToStringCopy(ctx, av, &mut exc);
        if s.is_null() {
          continue;
        }
        let m = JSStringGetMaximumUTF8CStringSize(s);
        let mut nbuf = vec![0u8; m];
        JSStringGetUTF8CString(s, nbuf.as_mut_ptr() as *mut _, m);
        JSStringRelease(s);
        arg_names.push(
          std::ffi::CStr::from_ptr(nbuf.as_ptr() as *const _)
            .to_string_lossy()
            .into_owned(),
        );
      }
    }

    // Source URL (deno hands it as the Source resource name): used both as the
    // JSEvaluateScript sourceURL (so the compiled CJS body carries a
    // SourceOrigin) and as the referrer for rewritten dynamic `import(...)`
    // inside CJS (e.g. next's bin doing `import("../cli/next-dev.js")`).
    let res_name = resource_name_of(ctx, source);
    let dyn_import = format!(
      "const __v82jsc_dynImport=(s,o)=>globalThis.__v82jsc_dynamicImport(s,{:?},o);\n",
      res_name
    );
    let body = rewrite_dynamic_import_calls(&body_str);
    let wrapped = format!(
      "(function({}) {{\n{dyn_import}{body}\n}})",
      arg_names.join(",")
    );
    let cstr = match std::ffi::CString::new(wrapped) {
      Ok(c) => c,
      Err(_) => return ptr::null(),
    };
    let js_src = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let res_cstr = std::ffi::CString::new(res_name).ok();
    let src_url_js = res_cstr
      .as_ref()
      .filter(|c| !c.as_bytes().is_empty())
      .map(|c| JSStringCreateWithUTF8CString(c.as_ptr()))
      .unwrap_or(ptr::null_mut());
    let result = compile_stat(cstr.as_bytes().len(), || {
      JSEvaluateScript(ctx, js_src, ptr::null_mut(), src_url_js, 1, &mut exc)
    });
    JSStringRelease(js_src);
    if !src_url_js.is_null() {
      JSStringRelease(src_url_js);
    }
    if result.is_null() {
      // Record the compile exception so deno's TryCatch sees has_caught() —
      // returning null with no pending exception trips its assert -> panic.
      let exc = if exc.is_null() {
        make_generic_error(ctx, "CompileFunction failed")
      } else {
        exc
      };
      crate::jsc::core::record_pending_exception(ctx, exc);
      return ptr::null();
    }
    intern_ctx::<Function>(ctx, result)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
  script: *const UnboundScript,
) -> *mut CachedData<'static> {
  // Encode the script's source to JSC bytecode so deno can persist it and
  // ConsumeCodeCache it on the next run (fast startup).
  #[cfg(feature = "vendor_jsc")]
  {
    let ctx = current_ctx();
    let src_val = jsval(script);
    if !ctx.is_null() && !src_val.is_null() {
      let src = unsafe { jsstring_to_rust(ctx, src_val) };
      if let (Ok(src_c), Ok(url_c)) = (
        std::ffi::CString::new(src),
        std::ffi::CString::new("script"),
      ) {
        let mut len: usize = 0;
        let p = unsafe {
          v82jsc_bytecode_encode(
            ctx,
            url_c.as_ptr(),
            src_c.as_ptr(),
            0,
            &mut len,
          )
        };
        if !p.is_null() && len > 0 {
          let bytes = unsafe { std::slice::from_raw_parts(p, len) }
            .to_vec()
            .into_boxed_slice();
          unsafe { v82jsc_bytecode_free(p) };
          return cached_data_from_bytes(frame_code_cache(&bytes));
        }
      }
    }
  }
  // Non-null fallback so deno's create_code_cache().ok_or_else doesn't error.
  make_placeholder_code_cache()
}

/// Wrap owned `bytes` in a v8 CachedData (buffer_policy=1: freed on DELETE).
#[cfg(feature = "vendor_jsc")]
fn cached_data_from_bytes(bytes: Box<[u8]>) -> *mut CachedData<'static> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  let length = bytes.len() as i32;
  let data = Box::into_raw(bytes) as *const u8;
  let boxed = Box::new(RawCachedData {
    data,
    length,
    rejected: false,
    buffer_policy: 1,
  });
  Box::into_raw(boxed) as *mut CachedData<'static>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
  _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
  make_placeholder_code_cache()
}

// Code caches this backend hands out are framed with a small header, so that
// `CachedData::CompatibilityCheck` can tell them from foreign bytes the way
// V8 checks its own magic number and version:
//
// | bytes | contents                                        |
// |-------|-------------------------------------------------|
// | 0..4  | `CODE_CACHE_MAGIC`                              |
// | 4..8  | `CachedDataVersionTag()`, little endian         |
// | 8..12 | payload length, little endian                   |
// | 12..  | payload: JSC bytecode, or empty for placeholder |

const CODE_CACHE_MAGIC: [u8; 4] = *b"v8xj";
const CODE_CACHE_HEADER_LEN: usize = 12;

// `v8::ScriptCompiler::CachedData::CompatibilityCheckResult` values.
const CACHE_CHECK_SUCCESS: i32 = 0;
const CACHE_CHECK_MAGIC_NUMBER_MISMATCH: i32 = 1;
const CACHE_CHECK_VERSION_MISMATCH: i32 = 2;
const CACHE_CHECK_INVALID_HEADER: i32 = 7;
const CACHE_CHECK_LENGTH_MISMATCH: i32 = 8;

fn frame_code_cache(payload: &[u8]) -> Box<[u8]> {
  let mut out = Vec::with_capacity(CODE_CACHE_HEADER_LEN + payload.len());
  out.extend_from_slice(&CODE_CACHE_MAGIC);
  out.extend_from_slice(
    &v8__ScriptCompiler__CachedDataVersionTag().to_le_bytes(),
  );
  out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  out.extend_from_slice(payload);
  out.into_boxed_slice()
}

/// The payload of a framed code cache, or the `CompatibilityCheckResult`
/// explaining why `bytes` is not one.
fn code_cache_payload(bytes: &[u8]) -> Result<&[u8], i32> {
  if bytes.len() < CODE_CACHE_HEADER_LEN {
    return Err(CACHE_CHECK_INVALID_HEADER);
  }
  let word = |i: usize| {
    u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
  };
  if bytes[0..4] != CODE_CACHE_MAGIC {
    return Err(CACHE_CHECK_MAGIC_NUMBER_MISMATCH);
  }
  if word(4) != v8__ScriptCompiler__CachedDataVersionTag() {
    return Err(CACHE_CHECK_VERSION_MISMATCH);
  }
  if word(8) as usize != bytes.len() - CODE_CACHE_HEADER_LEN {
    return Err(CACHE_CHECK_LENGTH_MISMATCH);
  }
  Ok(&bytes[CODE_CACHE_HEADER_LEN..])
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__CompatibilityCheck(
  this: *mut CachedData<'_>,
  _isolate: *mut RealIsolate,
) -> int {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  let Some(raw) = (unsafe { (this as *const RawCachedData).as_ref() }) else {
    return CACHE_CHECK_INVALID_HEADER;
  };
  if raw.data.is_null() || raw.length <= 0 {
    return CACHE_CHECK_INVALID_HEADER;
  }
  let bytes =
    unsafe { std::slice::from_raw_parts(raw.data, raw.length as usize) };
  match code_cache_payload(bytes) {
    Ok(_) => CACHE_CHECK_SUCCESS,
    Err(code) => code,
  }
}

/// `v8::ScriptCompiler::CompileOptionsIsValid`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileOptionsIsValid(
  compile_options: int,
) -> bool {
  let consume_code_cache = CompileOptions::ConsumeCodeCache.bits();
  let eager_compile = CompileOptions::EagerCompile.bits();
  let produce_and_consume_hints = CompileOptions::ProduceCompileHints.bits()
    | CompileOptions::ConsumeCompileHints.bits();
  // kConsumeCodeCache and kEagerCompile are each exclusive with every other
  // flag, and compile hints cannot be produced and consumed at once.
  if compile_options & consume_code_cache != 0
    && compile_options != consume_code_cache
  {
    return false;
  }
  if compile_options & eager_compile != 0 && compile_options != eager_compile {
    return false;
  }
  compile_options & produce_and_consume_hints != produce_and_consume_hints
}

pub(crate) fn make_placeholder_code_cache() -> *mut CachedData<'static> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }

  let v = frame_code_cache(&[]);
  let len = v.len() as i32;
  let data = Box::into_raw(v) as *const u8;
  let boxed = Box::new(RawCachedData {
    data,
    length: len,
    rejected: false,
    buffer_policy: 1,
  });
  Box::into_raw(boxed) as *mut CachedData<'static>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
  script: *const UnboundModuleScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  // GetUnboundModuleScript returns the Module value unchanged, so `script` is the
  // module handle; recover its `//# sourceMappingURL=` (extracted at compile).
  // deno reads this to register native source maps (inline data: or external).
  let url = module_state(script as *const Module)
    .and_then(|m| m.source_map_url.clone());
  if let Some(url) = url {
    if let Ok(c) = std::ffi::CString::new(url) {
      unsafe {
        let s = JSStringCreateWithUTF8CString(c.as_ptr());
        let v = JSValueMakeString(ctx, s);
        JSStringRelease(s);
        return intern_ctx::<Value>(ctx, v);
      }
    }
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

/// Byte offset of each import's specifier literal (the opening quote) in the
/// source, matching V8's `ModuleRequest::GetSourceOffset`. Specifiers are
/// resolved in requested-module order, advancing a cursor so repeated specifiers
/// map to successive occurrences. Returns 0 for any not found (best-effort).
/// ASCII offsets == V8's positions for these test sources.
fn compute_import_offsets(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<i32> {
  let bytes = text.as_bytes();
  let mut cursor = 0usize;
  let mut out = Vec::with_capacity(specifiers.len());
  for spec in specifiers {
    let mut found = -1i32;
    for quote in ['"', '\''] {
      let needle = format!("{quote}{spec}{quote}");
      if let Some(rel) = text[cursor..].find(&needle) {
        let pos = cursor + rel;
        // Prefer the earliest of the two quote styles.
        if found < 0 || (pos as i32) < found {
          found = pos as i32;
        }
      }
    }
    if found >= 0 {
      // Advance past this specifier so a later identical specifier resolves to
      // its own occurrence.
      cursor = (found as usize + 1).min(bytes.len());
      out.push(found);
    } else {
      out.push(0);
    }
  }
  out
}

/// Per-import `with { ... }` / `assert { ... }` attributes, parallel to
/// `specifiers`. JSC's module record only exposes a coarse type enum, so parse
/// the attribute clause that follows each specifier literal in the source.
/// Best-effort source scan (sufficient for deno's flat key:"value" attributes).
fn compute_import_attributes(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<Vec<(std::string::String, std::string::String)>> {
  let offsets = compute_import_offsets(text, specifiers);
  let mut out = Vec::with_capacity(specifiers.len());
  for (i, spec) in specifiers.iter().enumerate() {
    let mut attrs = Vec::new();
    let off = offsets[i];
    if off >= 0 {
      // Position just past the specifier's closing quote.
      let after = (off as usize + spec.len() + 2).min(text.len());
      let tail = text[after..].trim_start();
      // The attribute clause must immediately follow (modulo whitespace) and be
      // introduced by a `with`/`assert` keyword at a word boundary.
      let kw = ["with", "assert"].iter().find_map(|k| {
        tail.strip_prefix(*k).filter(|rest| {
          rest.starts_with(|c: char| c.is_whitespace() || c == '{')
        })
      });
      if let Some(rest) = kw {
        if let Some(open) = rest.find('{') {
          if let Some(close_rel) = rest[open + 1..].find('}') {
            let body = &rest[open + 1..open + 1 + close_rel];
            for pair in body.split(',') {
              if let Some((k, v)) = pair.split_once(':') {
                let key =
                  k.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                let val =
                  v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                if !key.is_empty() {
                  attrs.push((key, val));
                }
              }
            }
          }
        }
      }
    }
    out.push(attrs);
  }
  out
}

/// Extract the last `//# sourceMappingURL=<url>` (or legacy `//@`) magic comment
/// from module source, mirroring V8's UnboundModuleScript::GetSourceMappingURL.
/// V8 honours the LAST occurrence; the URL is the trimmed remainder of the line.
fn extract_source_mapping_url(text: &str) -> Option<std::string::String> {
  let mut found: Option<std::string::String> = None;
  for line in text.lines() {
    let t = line.trim();
    for prefix in ["//# sourceMappingURL=", "//@ sourceMappingURL="] {
      if let Some(rest) = t.strip_prefix(prefix) {
        let url = rest.trim();
        if !url.is_empty() {
          found = Some(url.to_string());
        }
      }
    }
  }
  found
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
  // Native ESM modules: report JSC's real record status so deno sees deps that
  // were evaluated by the graph cascade (CyclicModuleRecord::evaluate) rather
  // than via a per-module deno Evaluate call. Returns -1 for synthetic records,
  // which keep the wrapper status below.
  #[cfg(feature = "vendor_jsc")]
  if let Some(m) = module_state(this) {
    if !m.native.is_null() {
      let s = unsafe { v82jsc_module_status(m.native) };
      match s {
        0 => return ModuleStatus::Uninstantiated,
        1 => return ModuleStatus::Instantiating,
        2 => return ModuleStatus::Instantiated,
        3 => return ModuleStatus::Evaluating,
        4 => return ModuleStatus::Evaluated,
        _ => {}
      }
    }
  }
  match module_state(this) {
    Some(m) => match m.status {
      ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
      ModuleStatus::Instantiating => ModuleStatus::Instantiating,
      ModuleStatus::Instantiated => ModuleStatus::Instantiated,
      ModuleStatus::Evaluating => {
        // A module mid-body reports Evaluated so a re-entrant require() in a
        // cyclic graph gets its partial namespace instead of deno's "require
        // ES Module in a cycle" throw (matches the QuickJS backend).
        if EVAL_BODY_RUNNING.with(|s| s.borrow().contains(&this)) {
          ModuleStatus::Evaluated
        } else {
          ModuleStatus::Evaluating
        }
      }
      ModuleStatus::Evaluated => ModuleStatus::Evaluated,
      ModuleStatus::Errored => ModuleStatus::Errored,
    },

    None => ModuleStatus::Errored,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(
  _this: *const Module,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(
  this: *const Module,
) -> *const FixedArray {
  let ctx = module_state(this)
    .map(|m| m.ctx as JSContextRef)
    .unwrap_or_else(current_ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let specs: Vec<std::string::String> = module_state(this)
    .map(|m| m.import_specifiers.clone())
    .unwrap_or_default();
  let offsets: Vec<i32> = module_state(this)
    .map(|m| m.import_offsets.clone())
    .unwrap_or_default();
  let attributes: Vec<Vec<(std::string::String, std::string::String)>> =
    module_state(this)
      .map(|m| m.import_attributes.clone())
      .unwrap_or_default();
  let native = module_state(this)
    .map(|m| m.native)
    .unwrap_or(ptr::null_mut());

  let mut elems: Vec<JSValueRef> = Vec::with_capacity(specs.len());
  unsafe {
    for (idx, spec) in specs.iter().enumerate() {
      let req = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      if let Ok(cspec) = std::ffi::CString::new(spec.as_str()) {
        let sval_str = JSStringCreateWithUTF8CString(cspec.as_ptr());
        let sval = JSValueMakeString(ctx, sval_str);
        JSStringRelease(sval_str);
        let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
        JSObjectSetProperty(ctx, req, key, sval, 0, ptr::null_mut());
        JSStringRelease(key);
      }

      // Carry the specifier-literal source offset (deno's referrer_source_offset).
      let off = offsets.get(idx).copied().unwrap_or(0);
      let okey = JSStringCreateWithUTF8CString(c"__source_offset".as_ptr());
      JSObjectSetProperty(
        ctx,
        req,
        okey,
        JSValueMakeNumber(ctx, off as f64),
        1 << 1,
        ptr::null_mut(),
      );
      JSStringRelease(okey);

      // Carry the import's `with { ... }` attributes (source-extracted) as a flat
      // [key, value, key, value, ...] string array for GetImportAttributes.
      if let Some(pairs) = attributes.get(idx) {
        if !pairs.is_empty() {
          let mut kv: Vec<JSValueRef> = Vec::with_capacity(pairs.len() * 2);
          for (k, v) in pairs {
            for s in [k, v] {
              if let Ok(cs) = std::ffi::CString::new(s.as_str()) {
                let js = JSStringCreateWithUTF8CString(cs.as_ptr());
                kv.push(JSValueMakeString(ctx, js));
                JSStringRelease(js);
              } else {
                kv.push(JSValueMakeString(
                  ctx,
                  JSStringCreateWithUTF8CString(c"".as_ptr()),
                ));
              }
            }
          }
          let arr =
            JSObjectMakeArray(ctx, kv.len(), kv.as_ptr(), ptr::null_mut());
          if !arr.is_null() {
            let akey = JSStringCreateWithUTF8CString(c"__attrs".as_ptr());
            JSObjectSetProperty(
              ctx,
              req,
              akey,
              arr as JSValueRef,
              1 << 1,
              ptr::null_mut(),
            );
            JSStringRelease(akey);
          }
        }
      }

      // Carry the native import-attribute type (3=JSON, 2=wasm) so
      // GetImportAttributes can surface `with { type: ... }` to deno.
      #[cfg(feature = "vendor_jsc")]
      if !native.is_null() {
        let at = v82jsc_module_request_attr_type(native, idx as i32);
        if at != 0 {
          let akey = JSStringCreateWithUTF8CString(c"__attr_type".as_ptr());
          JSObjectSetProperty(
            ctx,
            req,
            akey,
            JSValueMakeNumber(ctx, at as f64),
            1 << 1,
            ptr::null_mut(),
          );
          JSStringRelease(akey);
        }
      }

      let mark =
        JSStringCreateWithUTF8CString(c"__v8jsc_module_request".as_ptr());
      JSObjectSetProperty(
        ctx,
        req,
        mark,
        JSValueMakeBoolean(ctx, true),
        1 << 1,
        ptr::null_mut(),
      );
      JSStringRelease(mark);
      elems.push(req as JSValueRef);
    }
    let mut exc: JSValueRef = ptr::null();
    let arr = JSObjectMakeArray(ctx, elems.len(), elems.as_ptr(), &mut exc);
    if arr.is_null() {
      return ptr::null();
    }
    intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
  _this: *const Module,
  _offset: int,
  out: *mut Location,
) {
  if !out.is_null() {
    unsafe { ptr::write_bytes(out as *mut u8, 0u8, size_of::<Location>()) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(
  this: *const Module,
) -> *const Value {
  // Native path: ask the JSModuleRecord for its real namespace object (valid
  // after link). Cache it on the wrapper so repeated calls are cheap.
  #[cfg(feature = "vendor_jsc")]
  if let Some(m) = module_state(this) {
    if !m.native.is_null() {
      let ctx = m.ctx as JSContextRef;
      if m.namespace.is_null() {
        let ns = unsafe { v82jsc_module_namespace(ctx, m.native) };
        if !ns.is_null() {
          unsafe { JSValueProtect(ctx, ns) };
          m.namespace = ns as JSObjectRef;
        }
      }
      if !m.namespace.is_null() {
        return intern_ctx::<Value>(ctx, m.namespace as JSValueRef);
      }
      return ptr::null();
    }
  }
  match module_state(this) {
    Some(m) if !m.namespace.is_null() => {
      intern_ctx::<Value>(m.ctx as JSContextRef, m.namespace as JSValueRef)
    }
    _ => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
  _this: *const Module,
  _phase: ModuleImportPhase,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
  _this: *const Module,
  _context: *const Context,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(_this: *const Module) -> int {
  (_this as usize as int) ^ 0x4d4f_44
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
  this: *const Module,
  context: *const Context,
  cb: ResolveModuleCallback,
  _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;

  let iso = current_iso();
  let Some(m) = module_state(this) else {
    return MaybeBool::JustFalse;
  };
  if !matches!(m.status, ModuleStatus::Uninstantiated) {
    return MaybeBool::JustTrue;
  }

  // Native JSModuleRecord path: DFS-resolve every edge (deno's callback) and
  // register each dependency in the parent's loaded-modules map, then link the
  // whole graph through JSC's real linker.
  #[cfg(feature = "vendor_jsc")]
  if !m.native.is_null() {
    let linkable = native_populate(context, cb, _source_callback, this);
    crate::jsc::core::restore_current(iso);
    let root_native = module_state(this)
      .map(|m| m.native)
      .unwrap_or(ptr::null_mut());
    // Only hand the graph to JSC's linker when EVERY edge resolved to a native
    // record; a synthetic/virtual dep would make link() RELEASE_ASSERT.
    let ok = linkable
      && !root_native.is_null()
      && unsafe { v82jsc_module_link(ctx, root_native) };
    if !ok {
      // Surface a pending exception so deno's instantiate returns Err instead of
      // asserting on the Errored status (mod_evaluate expects Instantiated).
      let msg = if !linkable {
        "v82jsc: native module graph has a non-ESM (synthetic) dependency"
      } else {
        "v82jsc: native module link failed"
      };
      unsafe {
        let e = make_generic_error(ctx, msg);
        crate::jsc::core::record_pending_exception(ctx, e);
      }
    }
    if let Some(m) = module_state(this) {
      m.status = if ok {
        ModuleStatus::Instantiated
      } else {
        ModuleStatus::Errored
      };
    }
    // On failure return Nothing (empty Maybe = exception occurred) so deno's
    // instantiate reads the pending exception and returns Err — JustFalse would
    // be Some(false), which deno treats as success and then asserts on status.
    return if ok {
      MaybeBool::JustTrue
    } else {
      MaybeBool::Nothing
    };
  }

  m.status = ModuleStatus::Instantiating;
  let specs = m.import_specifiers.clone();

  let mut deps: Vec<*const Module> = Vec::new();
  for spec in &specs {
    crate::jsc::core::restore_current(iso);
    let dep = unsafe { resolve_dependency(context, cb, this, spec, 0) };
    if dep.is_null() {
      continue;
    }

    if let Some(dm) = module_state(dep) {
      if matches!(dm.status, ModuleStatus::Uninstantiated) {
        let _ =
          v8__Module__InstantiateModule(dep, context, cb, _source_callback);
      }
    }
    deps.push(dep);
  }
  crate::jsc::core::restore_current(iso);
  if let Some(m) = module_state(this) {
    m.dependencies = deps;
    m.status = ModuleStatus::Instantiated;
  }
  MaybeBool::JustTrue
}

unsafe fn resolve_dependency(
  context: *const Context,
  cb: ResolveModuleCallback,
  referrer: *const Module,
  spec: &str,
  attr_type: i32,
) -> *const Module {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let cspec = match std::ffi::CString::new(spec) {
    Ok(c) => c,
    Err(_) => return ptr::null(),
  };
  let spec_str = unsafe { JSStringCreateWithUTF8CString(cspec.as_ptr()) };
  let spec_val = unsafe { JSValueMakeString(ctx, spec_str) };
  unsafe { JSStringRelease(spec_str) };
  let spec_handle = intern_ctx::<V8String>(ctx, spec_val);

  let mut exc: JSValueRef = ptr::null();
  // deno's resolve callback reads import attributes as StaticImport triples
  // [key, value, source_offset] and derives the requested module type, which it
  // matches against the request — so a JSON/wasm edge must carry its `type`.
  let type_str = match attr_type {
    3 => Some("json"),
    2 => Some("webassembly"),
    _ => None,
  };
  let attrs_arr = unsafe {
    if let Some(ts) = type_str {
      let mk = |s: &str| -> JSValueRef {
        let c = std::ffi::CString::new(s).unwrap();
        let js = JSStringCreateWithUTF8CString(c.as_ptr());
        let v = JSValueMakeString(ctx, js);
        JSStringRelease(js);
        v
      };
      let triple = [mk("type"), mk(ts), JSValueMakeNumber(ctx, 0.0)];
      JSObjectMakeArray(ctx, 3, triple.as_ptr(), &mut exc)
    } else {
      JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc)
    }
  };
  let attrs_handle = intern_ctx::<FixedArray>(ctx, attrs_arr as JSValueRef);

  let ret = unsafe {
    let ctx_l = crate::Local::from_raw(context).unwrap();
    let spec_l = crate::Local::from_raw(spec_handle).unwrap();
    let attrs_l = crate::Local::from_raw(attrs_handle).unwrap();
    let ref_l = crate::Local::from_raw(referrer).unwrap();
    cb(ctx_l, spec_l, attrs_l, ref_l)
  };

  unsafe { *(&ret as *const ResolveModuleCallbackRet as *const *const Module) }
}

/// DFS the native module graph rooted at `this`: resolve each import via deno's
/// callback, register the dep in `this`'s native loaded-modules map, recurse,
/// and record the deps + Instantiated status for GetStatus/IsGraphAsync. Does
/// NOT link (the caller links the root once, which recurses through JSC).
#[cfg(feature = "vendor_jsc")]
fn native_populate(
  context: *const Context,
  cb: ResolveModuleCallback,
  source_cb: Option<ResolveSourceCallback>,
  this: *const Module,
) -> bool {
  let ctx = ctx_of(context) as JSContextRef;
  let iso = current_iso();
  let Some(m) = module_state(this) else {
    return false;
  };
  if !matches!(m.status, ModuleStatus::Uninstantiated) {
    return !m.native.is_null(); // already visited (shared dep or cycle)
  }
  m.status = ModuleStatus::Instantiating;
  let specs = m.import_specifiers.clone();
  let this_native = m.native;

  let mut deps: Vec<*const Module> = Vec::new();
  let mut linkable = !this_native.is_null();
  for (idx, spec) in specs.iter().enumerate() {
    crate::jsc::core::restore_current(iso);
    let attr_type = if this_native.is_null() {
      0
    } else {
      unsafe { v82jsc_module_request_attr_type(this_native, idx as i32) }
    };
    let dep = unsafe { resolve_dependency(context, cb, this, spec, attr_type) };
    if dep.is_null() {
      linkable = false;
      continue;
    }
    // Recurse first so the dep's own edges are registered before it links.
    let dep_linkable = native_populate(context, cb, source_cb, dep);
    let dep_native = module_state(dep)
      .map(|d| d.native)
      .unwrap_or(ptr::null_mut());
    if !this_native.is_null() && !dep_native.is_null() {
      if let Ok(cspec) = std::ffi::CString::new(spec.as_str()) {
        crate::jsc::core::restore_current(iso);
        unsafe {
          v82jsc_module_add_dependency(
            ctx,
            this_native,
            cspec.as_ptr(),
            dep_native,
          );
        }
      }
    } else {
      // A synthetic/virtual dep (JSON, node: builtin) has no native record —
      // this graph can't be linked through JSC.
      linkable = false;
    }
    // A synthetic dep's export values are set by deno's eval_steps, which JSC's
    // no-op SyntheticModuleRecord::evaluate won't drive during the root's graph
    // cascade. Run them now so the values are in the native env before any
    // importer reads them. (eval_steps for JSON/ops are imperative + order-free.)
    let dep_is_synthetic = module_state(dep)
      .map(|d| d.eval_steps.is_some() && !d.native.is_null())
      .unwrap_or(false);
    if dep_is_synthetic {
      let needs_eval = module_state(dep)
        .map(|d| !matches!(d.status, ModuleStatus::Evaluated))
        .unwrap_or(false);
      if needs_eval {
        crate::jsc::core::restore_current(iso);
        let _ = v8__Module__Evaluate(dep, context);
        crate::jsc::core::restore_current(iso);
      }
    }
    linkable = linkable && dep_linkable;
    deps.push(dep);
  }
  if let Some(m) = module_state(this) {
    m.dependencies = deps;
    m.status = ModuleStatus::Instantiated;
  }
  linkable
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context) as JSContextRef;
  let Some(m) = module_state(this) else {
    return ptr::null();
  };

  if matches!(m.status, ModuleStatus::Evaluated) {
    return make_resolved_promise(ctx);
  }

  // Native JSModuleRecord path: JSC's evaluate() drives the whole linked graph
  // (deps first, cycles handled internally) and returns a promise per the
  // top-level-await spec. No manual dep walk. Synthetic modules (eval_steps set)
  // take the eval_steps path below — that's what fills their export values
  // (which we mirror into the native SyntheticModuleRecord env).
  #[cfg(feature = "vendor_jsc")]
  if !m.native.is_null() && m.eval_steps.is_none() {
    let native = m.native;
    m.status = ModuleStatus::Evaluating;
    EVAL_BODY_RUNNING.with(|s| s.borrow_mut().push(this));
    let result = unsafe { v82jsc_module_evaluate(ctx, native) };
    EVAL_BODY_RUNNING.with(|s| {
      let mut v = s.borrow_mut();
      if let Some(pos) = v.iter().rposition(|&p| p == this) {
        v.remove(pos);
      }
    });
    if let Some(m) = module_state(this) {
      m.status = ModuleStatus::Evaluated;
    }
    if result.is_null() {
      return make_resolved_promise(ctx);
    }
    if unsafe { JSValueIsObject(ctx, result) } {
      crate::jsc::exception::track_promise_pub(ctx, result as JSObjectRef);
      return intern_ctx::<Value>(ctx, result);
    }
    return make_resolved_promise(ctx);
  }

  m.status = ModuleStatus::Evaluating;

  let iso = current_iso();
  let deps = m.dependencies.clone();
  let mut dep_promises: Vec<JSValueRef> = Vec::new();
  for dep in &deps {
    if dep.is_null() {
      continue;
    }
    let dep_async = v8__Module__IsGraphAsync(*dep);
    if let Some(dm) = module_state(*dep) {
      // Skip a dep already done, OR currently mid-evaluation up the call stack
      // (circular import: A imports B imports A). Re-entering an `Evaluating`
      // module recurses forever; its live bindings are already wired by
      // InstantiateModule, so the cycle resolves without a re-eval.
      if matches!(dm.status, ModuleStatus::Evaluating)
        || (matches!(dm.status, ModuleStatus::Evaluated) && !dep_async)
      {
        continue;
      }
    }
    let p = v8__Module__Evaluate(*dep, context);
    if dep_async && !p.is_null() {
      dep_promises.push(jsval(p));
    }
  }
  crate::jsc::core::restore_current(iso);

  let Some(m) = module_state(this) else {
    return ptr::null();
  };

  if !m.specifier.is_empty() {
    unsafe { register_module_namespace(ctx, &m.specifier, m.namespace) };
  }

  {
    let raw_specs = m.import_specifiers.clone();
    let deps2 = m.dependencies.clone();
    for (raw, dep) in raw_specs.iter().zip(deps2.iter()) {
      if dep.is_null() {
        continue;
      }
      if let Some(dm) = module_state(*dep) {
        if !raw.is_empty() && !dm.namespace.is_null() {
          unsafe { register_module_namespace(ctx, raw, dm.namespace) };
        }
      }
    }
  }

  if let Some(eval_steps) = m.eval_steps {
    let ret = unsafe {
      let ctx_l = crate::Local::from_raw(context as *const Context).unwrap();
      let mod_l = crate::Local::from_raw(this).unwrap();
      eval_steps(ctx_l, mod_l)
    };
    if let Some(m) = module_state(this) {
      m.status = ModuleStatus::Evaluated;
    }
    let promise = unsafe {
      *(&ret as *const SyntheticModuleEvaluationStepsRet as *const *const Value)
    };
    if !promise.is_null() {
      return promise;
    }
    return make_resolved_promise(ctx);
  }

  if let Some(src) = m.source.clone() {
    let is_async = m.is_async || !dep_promises.is_empty();
    let namespace = m.namespace;
    let source_url = m.specifier.clone();
    let meta = unsafe { build_import_meta(context, this) };
    // Mark the body as running: while it executes, GetStatus reports this
    // module as Evaluated (not Evaluating) so a re-entrant require()/import in a
    // cyclic graph (node:process's loadExtScript chain) gets the partial
    // namespace instead of deno's "require ES Module in a cycle" throw. Status
    // stays Evaluating internally, so the async/promise return path is intact.
    EVAL_BODY_RUNNING.with(|s| s.borrow_mut().push(this));
    let eval_result = unsafe {
      eval_module_source(
        ctx,
        &src,
        namespace,
        meta,
        is_async,
        &dep_promises,
        &source_url,
      )
    };
    EVAL_BODY_RUNNING.with(|s| {
      let mut v = s.borrow_mut();
      if let Some(pos) = v.iter().rposition(|&p| p == this) {
        v.remove(pos);
      }
    });
    match eval_result {
      Ok(promise) => {
        if let Some(m) = module_state(this) {
          m.status = ModuleStatus::Evaluated;
        }

        if !promise.is_null() {
          crate::jsc::exception::track_promise_pub(ctx, promise as JSObjectRef);
          return intern_ctx::<Value>(ctx, promise);
        }
      }
      Err(exc) => {
        if let Some(m) = module_state(this) {
          m.status = ModuleStatus::Errored;
        }
        return make_rejected_promise(ctx, exc);
      }
    }
  } else if let Some(m) = module_state(this) {
    m.status = ModuleStatus::Evaluated;
  }
  make_resolved_promise(ctx)
}

fn make_resolved_promise(ctx: JSContextRef) -> *const Value {
  if ctx.is_null() {
    return ptr::null();
  }
  let src = b"Promise.resolve(undefined)\0";
  let mut exc: JSValueRef = ptr::null();
  let js = unsafe {
    JSStringCreateWithUTF8CString(src.as_ptr() as *const std::os::raw::c_char)
  };
  let p = unsafe {
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
  };
  unsafe { JSStringRelease(js) };
  if p.is_null() {
    let v = unsafe { JSValueMakeUndefined(ctx) };
    return intern_ctx::<Value>(ctx, v);
  }

  crate::jsc::exception::track_promise_pub(ctx, p as JSObjectRef);
  intern_ctx::<Value>(ctx, p)
}

unsafe fn eval_module_source(
  ctx: JSContextRef,
  rewritten: &str,
  namespace: JSObjectRef,
  meta: JSValueRef,
  is_async: bool,
  dep_promises: &[JSValueRef],
  source_url: &str,
) -> Result<JSValueRef, JSValueRef> {
  let fail =
    |ctx: JSContextRef, exc: JSValueRef| -> Result<JSValueRef, JSValueRef> {
      unsafe { report_module_exception(ctx, exc) };

      let e = if exc.is_null() {
        make_generic_error(ctx, "module evaluation failed")
      } else {
        exc
      };
      Err(e)
    };
  let kw = if is_async {
    "async function"
  } else {
    "function"
  };

  let await_deps = if is_async && !dep_promises.is_empty() {
    "await Promise.all(__deps);\n"
  } else {
    ""
  };
  // Per-module closure that rewrite_dynamic_import_calls()'d `import(...)` sites
  // call; forwards to the host bridge with this module's URL as the referrer.
  let dyn_import = format!(
    "const __v82jsc_dynImport=(s,o)=>globalThis.__v82jsc_dynamicImport(s,{:?},o);\n",
    source_url
  );
  let body = rewrite_dynamic_import_calls(rewritten);
  let wrapped = format!(
    "({kw}(__ns, __v8jsc_meta, __deps){{\n{dyn_import}{await_deps}{body}\n}})"
  );
  let cstr = match std::ffi::CString::new(wrapped) {
    Ok(c) => c,
    Err(_) => return fail(ctx, ptr::null()),
  };
  let mut exc: JSValueRef = ptr::null();
  let js = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr()) };
  // Pass the module's URL as JSEvaluateScript's sourceURL so JSC records a
  // SourceOrigin. The dynamic-import hook (moduleLoaderImportModule) reads
  // sourceOrigin.string() as the referrer; without it, deno can't resolve
  // relative `import()` specifiers (e.g. next's `import("../cli/next-dev.js")`).
  let src_url_cstr = std::ffi::CString::new(source_url).ok();
  let src_url_js = src_url_cstr
    .as_ref()
    .map(|c| unsafe { JSStringCreateWithUTF8CString(c.as_ptr()) })
    .unwrap_or(ptr::null_mut());
  let f = unsafe {
    JSEvaluateScript(ctx, js, ptr::null_mut(), src_url_js, 1, &mut exc)
  };
  unsafe { JSStringRelease(js) };
  if !src_url_js.is_null() {
    unsafe { JSStringRelease(src_url_js) };
  }
  if f.is_null() {
    return fail(ctx, exc);
  }
  let fobj = unsafe { JSValueToObject(ctx, f, &mut exc) };
  if fobj.is_null() {
    return fail(ctx, exc);
  }
  let meta = if meta.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    meta
  };
  let deps_arr = unsafe {
    JSObjectMakeArray(ctx, dep_promises.len(), dep_promises.as_ptr(), &mut exc)
  };
  let deps_val = if deps_arr.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    deps_arr as JSValueRef
  };
  let args = [namespace as JSValueRef, meta, deps_val];
  let r = unsafe {
    JSObjectCallAsFunction(
      ctx,
      fobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    )
  };
  if is_async {
    if r.is_null() || !exc.is_null() {
      return fail(ctx, exc);
    }
    return Ok(r);
  }
  if r.is_null() || !exc.is_null() {
    return fail(ctx, exc);
  }
  Ok(ptr::null())
}

unsafe fn make_generic_error(ctx: JSContextRef, message: &str) -> JSValueRef {
  let mut exc: JSValueRef = ptr::null();
  let msg = std::ffi::CString::new(message).unwrap_or_default();
  let s = unsafe { JSStringCreateWithUTF8CString(msg.as_ptr()) };
  let arg = unsafe { JSValueMakeString(ctx, s) };
  unsafe { JSStringRelease(s) };
  let args = [arg];
  let e = unsafe { JSObjectMakeError(ctx, 1, args.as_ptr(), &mut exc) };
  if e.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    e as JSValueRef
  }
}

fn make_rejected_promise(ctx: JSContextRef, exc: JSValueRef) -> *const Value {
  if ctx.is_null() {
    return ptr::null();
  }
  let exc = if exc.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    exc
  };

  let mut e: JSValueRef = ptr::null();
  let p = unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let pkey = JSStringCreateWithUTF8CString(c"Promise".as_ptr());
    let promise_ctor = JSObjectGetProperty(ctx, global, pkey, &mut e);
    JSStringRelease(pkey);
    if promise_ctor.is_null() || !JSValueIsObject(ctx, promise_ctor) {
      return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
    }
    let rkey = JSStringCreateWithUTF8CString(c"reject".as_ptr());
    let reject =
      JSObjectGetProperty(ctx, promise_ctor as JSObjectRef, rkey, &mut e);
    JSStringRelease(rkey);
    if reject.is_null() || !JSValueIsObject(ctx, reject) {
      return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
    }
    let args = [exc];
    JSObjectCallAsFunction(
      ctx,
      reject as JSObjectRef,
      promise_ctor as JSObjectRef,
      1,
      args.as_ptr(),
      &mut e,
    )
  };
  if p.is_null() {
    return intern_ctx::<Value>(ctx, unsafe { JSValueMakeUndefined(ctx) });
  }
  crate::jsc::exception::track_promise_pub(ctx, p as JSObjectRef);
  intern_ctx::<Value>(ctx, p)
}

/// Install live re-export getters on `namespace`: for each `export { local as
/// exported } from spec`, define `namespace[exported]` as a getter returning
/// `__v8jsc_modules[spec][local]` evaluated lazily. Lets a barrel's re-exports
/// resolve to the source module even before the barrel's own body runs.
unsafe fn install_reexport_getters(
  ctx: JSContextRef,
  namespace: JSObjectRef,
  reexports: &[(
    std::string::String,
    std::string::String,
    std::string::String,
  )],
) {
  if ctx.is_null() || namespace.is_null() || reexports.is_empty() {
    return;
  }
  let mut body = std::string::String::from("(function(__ns){\n");
  for (exported, spec, local) in reexports {
    let value = if local == "*" {
      format!("((globalThis.__v8jsc_modules||{{}})[{spec:?}]||{{}})")
    } else {
      format!("((globalThis.__v8jsc_modules||{{}})[{spec:?}]||{{}})[{local:?}]")
    };
    body.push_str(&format!(
      "Object.defineProperty(__ns, {exported:?}, {{configurable:true, enumerable:true, get:function(){{ return {value}; }}}});\n"
    ));
  }
  body.push_str("})");
  let Ok(cstr) = std::ffi::CString::new(body) else {
    return;
  };
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let js = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let f =
      JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
    if f.is_null() || !JSValueIsObject(ctx, f) {
      return;
    }
    let args = [namespace as JSValueRef];
    JSObjectCallAsFunction(
      ctx,
      f as JSObjectRef,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
  }
}

unsafe fn register_module_namespace(
  ctx: JSContextRef,
  specifier: &str,
  namespace: JSObjectRef,
) {
  if ctx.is_null() || namespace.is_null() || specifier.is_empty() {
    return;
  }
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let mut exc: JSValueRef = ptr::null();

    let reg_key = JSStringCreateWithUTF8CString(c"__v8jsc_modules".as_ptr());
    let mut reg = JSObjectGetProperty(ctx, global, reg_key, &mut exc);
    let reg_obj = if reg.is_null() || JSValueIsUndefined(ctx, reg) {
      let o = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      JSObjectSetProperty(
        ctx,
        global,
        reg_key,
        o as JSValueRef,
        1 << 1,
        &mut exc,
      );
      o
    } else {
      JSValueToObject(ctx, reg, &mut exc)
    };
    JSStringRelease(reg_key);
    if reg_obj.is_null() {
      return;
    }
    if let Ok(cspec) = std::ffi::CString::new(specifier) {
      let spec_key = JSStringCreateWithUTF8CString(cspec.as_ptr());
      JSObjectSetProperty(
        ctx,
        reg_obj,
        spec_key,
        namespace as JSValueRef,
        0,
        &mut exc,
      );
      JSStringRelease(spec_key);
    }
  }
}

unsafe fn build_import_meta(
  context: *const Context,
  module: *const Module,
) -> JSValueRef {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let meta = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  let iso = current_iso();
  if iso.is_null() {
    return meta as JSValueRef;
  }
  let cb = iso_state(iso).import_meta_cb;
  if let Some(cb) = cb {
    unsafe {
      let ctx_l = crate::Local::from_raw(context).unwrap();
      let mod_l = crate::Local::from_raw(module).unwrap();
      let meta_l =
        crate::Local::<Object>::from_raw(meta as *const Object).unwrap();
      cb(ctx_l, mod_l, meta_l);
    }
  }
  meta as JSValueRef
}

unsafe fn report_module_exception(ctx: JSContextRef, exc: JSValueRef) {
  if exc.is_null() {
    return;
  }
  crate::jsc::core::record_pending_exception(ctx, exc);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
  fn graph_async(this: *const Module, seen: &mut Vec<*const Module>) -> bool {
    if this.is_null() || seen.contains(&this) {
      return false;
    }
    seen.push(this);
    match module_state(this) {
      Some(m) => {
        if m.is_async {
          return true;
        }
        let deps = m.dependencies.clone();
        deps.iter().any(|d| graph_async(*d, seen))
      }
      None => false,
    }
  }
  let mut seen = Vec::new();
  graph_async(this, &mut seen)
}

/// Whether this module's own source uses top-level `await`, decided when it
/// was compiled. Unlike `IsGraphAsync`, dependencies do not count.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__HasTopLevelAwait(this: *const Module) -> bool {
  module_state(this).is_some_and(|m| m.is_async)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
  module_state(this).is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
  isolate: *const RealIsolate,
  module_name: *const V8String,
  export_names_len: usize,
  export_names_raw: *const *const V8String,
  evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
  let st = iso_state(isolate as *mut RealIsolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };
  let specifier = unsafe { jsstring_to_rust(ctx, jsval(module_name)) };

  let mut names: Vec<std::string::String> =
    Vec::with_capacity(export_names_len);
  if !export_names_raw.is_null() {
    for i in 0..export_names_len {
      let nptr = unsafe { *export_names_raw.add(i) };
      let nv = jsval(nptr);
      if nv.is_null() {
        continue;
      }
      let mut exc: JSValueRef = ptr::null();
      let s = unsafe { JSValueToStringCopy(ctx, nv, &mut exc) };
      if s.is_null() {
        continue;
      }
      let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
      let mut buf = vec![0u8; max];
      let n =
        unsafe { JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max) };
      unsafe { JSStringRelease(s) };
      if n > 0 {
        buf.truncate(n - 1);
        if let Ok(name) = std::string::String::from_utf8(buf) {
          names.push(name);
        }
      }
    }
  }

  let namespace =
    unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

  let steps: SyntheticModuleEvaluationSteps<'static> =
    unsafe { std::mem::transmute(evaluation_steps) };

  // Back the synthetic module with a real JSC SyntheticModuleRecord so native
  // ESM importers can link to it. Export values start undefined and are filled
  // by SetSyntheticModuleExport (called from eval_steps) into the record's
  // module environment (live bindings).
  let native = {
    #[cfg(feature = "vendor_jsc")]
    {
      if native_modules_enabled() && native_eligible(&specifier) {
        let cnames: Vec<std::ffi::CString> = names
          .iter()
          .filter_map(|n| std::ffi::CString::new(n.as_str()).ok())
          .collect();
        let ptrs: Vec<*const std::os::raw::c_char> =
          cnames.iter().map(|c| c.as_ptr()).collect();
        let url = std::ffi::CString::new(specifier.as_str()).ok();
        match url {
          Some(u) => unsafe {
            v82jsc_synthetic_create(
              ctx,
              u.as_ptr(),
              ptrs.as_ptr(),
              ptrs.len() as i32,
            )
          },
          None => ptr::null_mut(),
        }
      } else {
        ptr::null_mut()
      }
    }
    #[cfg(not(feature = "vendor_jsc"))]
    ptr::null_mut()
  };

  let state = Box::new(SyntheticModule {
    ctx: gctx,
    status: ModuleStatus::Uninstantiated,
    export_names: names,
    eval_steps: Some(steps),
    source: None,
    source_map_url: None,
    import_offsets: Vec::new(),
    import_attributes: Vec::new(),
    import_specifiers: Vec::new(),
    namespace,
    specifier,
    dependencies: Vec::new(),
    is_async: false,
    native,
  });
  let obj =
    unsafe { JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _) };
  intern_ctx::<Module>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
  this: *const Module,
  _isolate: *const RealIsolate,
  export_name: *const V8String,
  export_value: *const Value,
) -> MaybeBool {
  let Some(m) = module_state(this) else {
    return MaybeBool::JustFalse;
  };
  let ctx = m.ctx as JSContextRef;
  let name_v = jsval(export_name);
  let val = jsval(export_value);
  if ctx.is_null() || name_v.is_null() || m.namespace.is_null() {
    return MaybeBool::JustFalse;
  }
  let mut exc: JSValueRef = ptr::null();
  let key = unsafe { JSValueToStringCopy(ctx, name_v, &mut exc) };
  if key.is_null() {
    return MaybeBool::JustFalse;
  }
  unsafe {
    JSObjectSetProperty(ctx, m.namespace, key, val, 0, &mut exc);
    JSStringRelease(key);
  }

  // Mirror the export into the native SyntheticModuleRecord's module
  // environment so native ESM importers see the value (live binding).
  #[cfg(feature = "vendor_jsc")]
  if !m.native.is_null() {
    let name_str = unsafe { jsstring_to_rust(ctx, name_v) };
    if let Ok(cname) = std::ffi::CString::new(name_str) {
      unsafe {
        v82jsc_synthetic_set_export(ctx, m.native, cname.as_ptr(), val)
      };
    }
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
  this: *const Module,
) -> *const UnboundModuleScript {
  if this.is_null() {
    return ptr::null();
  }
  intern::<UnboundModuleScript>(jsval(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
  _this: *const Module,
  _isolate: *const RealIsolate,
  _out_vec: *mut StalledTopLevelAwaitMessage,
  _vec_len: usize,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(
  this: *const ModuleRequest,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return ptr::null();
    }
    intern_ctx::<V8String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetPhase(
  _this: *const ModuleRequest,
) -> ModuleImportPhase {
  ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(
  this: *const ModuleRequest,
) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let key = JSStringCreateWithUTF8CString(c"__source_offset".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return 0;
    }
    let n = JSValueToNumber(ctx, v, &mut exc);
    if n.is_nan() { 0 } else { n as int }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
  this: *const ModuleRequest,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  // V8 returns static-import attributes as [key, value, source_offset] triples.
  // Prefer the full source-extracted attribute set (`__attrs`, a flat [key,
  // value, ...] array set by GetModuleRequests) so deno sees ALL attributes
  // (`type: "text"/"bytes"/<custom>`, arbitrary keys). Fall back to the coarse
  // `__attr_type` (json/wasm) the native record exposes.
  let mut elems: Vec<JSValueRef> = Vec::new();
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let mk = |s: &str| -> JSValueRef {
      let c = std::ffi::CString::new(s).unwrap_or_default();
      let js = JSStringCreateWithUTF8CString(c.as_ptr());
      let v = JSValueMakeString(ctx, js);
      JSStringRelease(js);
      v
    };
    if !this.is_null() {
      let obj = jsval(this) as JSObjectRef;
      // Source-extracted attributes: flat [key, value, ...] array.
      let attrs_key = JSStringCreateWithUTF8CString(c"__attrs".as_ptr());
      let attrs_val = JSObjectGetProperty(ctx, obj, attrs_key, &mut exc);
      JSStringRelease(attrs_key);
      let attrs_arr = if !attrs_val.is_null() && JSValueIsObject(ctx, attrs_val)
      {
        Some(attrs_val as JSObjectRef)
      } else {
        None
      };
      if let Some(arr) = attrs_arr {
        let len_key = JSStringCreateWithUTF8CString(c"length".as_ptr());
        let len_val = JSObjectGetProperty(ctx, arr, len_key, &mut exc);
        JSStringRelease(len_key);
        let len = JSValueToNumber(ctx, len_val, &mut exc) as usize;
        let mut i = 0;
        while i + 1 < len {
          let k = JSObjectGetPropertyAtIndex(ctx, arr, i as u32, &mut exc);
          let v =
            JSObjectGetPropertyAtIndex(ctx, arr, (i + 1) as u32, &mut exc);
          elems.push(k);
          elems.push(v);
          elems.push(JSValueMakeNumber(ctx, 0.0)); // attribute source offset
          i += 2;
        }
      } else {
        // Fallback: coarse type enum from the native record.
        let akey = JSStringCreateWithUTF8CString(c"__attr_type".as_ptr());
        let at_val = JSObjectGetProperty(ctx, obj, akey, &mut exc);
        JSStringRelease(akey);
        let at = if at_val.is_null() {
          0.0
        } else {
          JSValueToNumber(ctx, at_val, &mut exc)
        };
        let type_str = match at as i32 {
          3 => Some("json"),
          2 => Some("webassembly"),
          _ => None,
        };
        if let Some(ts) = type_str {
          elems.push(mk("type"));
          elems.push(mk(ts));
          elems.push(JSValueMakeNumber(ctx, 0.0));
        }
      }
    }
    let arr = JSObjectMakeArray(ctx, elems.len(), elems.as_ptr(), &mut exc);
    if arr.is_null() {
      return ptr::null();
    }
    intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
    JSStringRelease(src_str);
    if !ok {
      return ptr::null();
    }
  }
  intern_ctx::<UnboundScript>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
  0x5643_4a53
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
  script: *const UnboundScript,
) -> *const Script {
  if script.is_null() {
    return ptr::null();
  }
  intern::<Script>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
  _script: *const UnboundScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSourceTextModule(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__ScriptId(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ResourceName(
  _origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ScriptId(
  _origin: *const std::os::raw::c_void,
) -> i32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__SourceMapUrl(
  _origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}
