//! Hand-written, JSC-backed implementations of the v8 C-ABI surface.
//!
//! Symbols defined here are EXCLUDED from the auto-generated stubs in
//! `shims.rs` (see tools/gen_shims.sh). This file grows as the runtime path is
//! brought up on JavaScriptCore; everything not yet here is an `unimplemented!`
//! stub.

#![allow(non_snake_case)]

use crate::Platform;
use std::os::raw::{c_char, c_int};

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(_platform: *mut Platform) {
  // Enable JS features that v8 ships on-by-default but JSC gates behind
  // off-by-default options, so deno's globals/syntax match v8. JSC reads
  // `JSC_<option>` env vars at Options::initialize() (these are all
  // `Normal`-availability, honored in release), which runs lazily on first VM
  // creation — after this, before any isolate exists. Works for vendored and
  // system JSC alike (no C++ Options API). Don't clobber explicit overrides.
  //
  // - useExplicitResourceManagement: TC39 `using`/`await using` +
  //   Symbol.dispose/asyncDispose (deno's transpiler emits these verbatim).
  //   ONLY on the vendored (built-from-source, newer) WebKit — Apple's shipped
  //   `JavaScriptCore.framework` predates this option and JSC prints
  //   `ERROR: invalid option: JSC_useExplicitResourceManagement=1` to stderr at
  //   Options::initialize(). That raw fd-2 write interleaves into libtest's
  //   `test NAME ... ok` line and non-deterministically breaks the harness
  //   parser → baselined tests flap as "MISSING" and the sys-jsc cells flip
  //   pass/fail with no code change (denoland/divybot#653). The option is an
  //   unrecognized no-op on that JSC anyway, so skip it there.
  // - useSharedArrayBuffer: the `SharedArrayBuffer` global (deno exposes it;
  //   Workers/Atomics rely on it). Recognized on both, so always set.
  // - useFTLJIT=0: FTL's B3 backend optimizes a side-effect-free infinite loop
  //   (`for(;;){}`) into a bare machine loop with no `op_loop_hint` safe-point,
  //   so the execution-time-limit watchdog backing `Isolate::TerminateExecution`
  //   (see jsc/terminate.rs) can never interrupt it and the loop hangs forever.
  //   The DFG/baseline tiers keep the safe-point, so dropping just FTL makes such
  //   loops terminable while staying fast. (Allocating loops already hit GC
  //   safe-points.) vendor_jsc only — same stderr rejection as above.
  let mut opts: Vec<(&str, &str)> = vec![("JSC_useSharedArrayBuffer", "1")];
  #[cfg(feature = "vendor_jsc")]
  {
    opts.push(("JSC_useExplicitResourceManagement", "1"));
    opts.push(("JSC_useFTLJIT", "0"));
  }
  for (key, val) in opts {
    if std::env::var_os(key).is_none() {
      // SAFETY: called once at platform init, before any threads spawn a VM.
      unsafe {
        std::env::set_var(key, val);
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {}

/// WebAssembly runs on the vendored WAMR interpreter here, which detects
/// out-of-bounds accesses with explicit bounds checks instead of guard pages,
/// so there is no trap handler to install. Report failure, which is what V8
/// does on a build without trap-handler support.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__EnableWebAssemblyTrapHandler(
  _use_v8_signal_handler: bool,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
  if std::env::var_os("V82JSC_COMPILE_STATS").is_some() {
    use std::sync::atomic::Ordering;
    let bytes = crate::jsc::module::COMPILE_BYTES.load(Ordering::Relaxed);
    let nanos = crate::jsc::module::COMPILE_NANOS.load(Ordering::Relaxed);
    eprintln!(
      "[compile-stats] {} KB compiled in {:.1} ms",
      bytes / 1024,
      nanos as f64 / 1.0e6
    );
  }
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {}

// JSC seeds its own RNG; the v8 entropy-source hook is a no-op. Defined so
// `test_api_entropy_source.rs` links.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource(_callback: *const std::ffi::c_void) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(
  _flags: *const u8,
  _length: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
  argc: *mut c_int,
  _argv: *mut *mut c_char,
  _usage: *const c_char,
) {
  if !argc.is_null() {
    unsafe {
      if *argc > 1 {
        *argc = 1;
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
  // Report the V8 version string our vendored rusty_v8 surface was generated
  // against (`v8::VERSION_STRING`), so `V8::get_version()` round-trips exactly.
  // This is a compat shim emulating V8; downstream code (e.g. Deno) compares
  // against the V8 version, not JavaScriptCore's. (`jsc_version_string` is kept
  // for diagnostics but no longer drives the reported version.)
  use std::sync::OnceLock;
  static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
  VERSION
    .get_or_init(|| std::ffi::CString::new(crate::VERSION_STRING).unwrap())
    .as_ptr()
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn jsc_version_string() -> std::ffi::CString {
  use std::os::raw::c_void;
  type CFRef = *const c_void;
  #[allow(non_snake_case)]
  unsafe extern "C" {
    fn CFBundleGetBundleWithIdentifier(id: CFRef) -> CFRef;
    fn CFBundleGetValueForInfoDictionaryKey(b: CFRef, key: CFRef) -> CFRef;
    fn CFStringCreateWithCString(
      alloc: CFRef,
      s: *const c_char,
      enc: u32,
    ) -> CFRef;
    fn CFStringGetCString(
      s: CFRef,
      buf: *mut c_char,
      len: isize,
      enc: u32,
    ) -> bool;
    fn CFRelease(r: CFRef);
  }
  const UTF8: u32 = 0x0800_0100;
  let ver = unsafe {
    let id = CFStringCreateWithCString(
      std::ptr::null(),
      c"com.apple.JavaScriptCore".as_ptr(),
      UTF8,
    );
    let bundle = CFBundleGetBundleWithIdentifier(id);
    if !id.is_null() {
      CFRelease(id);
    }
    let mut out = String::new();
    if !bundle.is_null() {
      let key = CFStringCreateWithCString(
        std::ptr::null(),
        c"CFBundleVersion".as_ptr(),
        UTF8,
      );
      let val = CFBundleGetValueForInfoDictionaryKey(bundle, key);
      if !key.is_null() {
        CFRelease(key);
      }
      if !val.is_null() {
        let mut buf = [0i8; 128];
        if CFStringGetCString(val, buf.as_mut_ptr(), buf.len() as isize, UTF8) {
          out = std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        }
      }
    }
    out
  };
  let label = if ver.is_empty() {
    "JavaScriptCore".to_string()
  } else {
    format!("{ver} (JavaScriptCore)")
  };
  std::ffi::CString::new(label).unwrap()
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn jsc_version_string() -> std::ffi::CString {
  std::ffi::CString::new("JavaScriptCore").unwrap()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewUnprotectedDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
  _idle_task_support: bool,
) -> *mut Platform {
  Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewCustomPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
  _unprotected: bool,
  _context: *mut std::ffi::c_void,
) -> *mut Platform {
  Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop(
  _platform: *mut Platform,
  _isolate: *mut std::ffi::c_void,
  _wait_for_work: bool,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
  _platform: *mut Platform,
  _isolate: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE(this: *mut Platform) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut u8)) };
  }
}
