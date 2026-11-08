//! Inert V8 global-init / Platform shims for the QuickJS backend.
//!
//! QuickJS initializes lazily and has no libplatform/task-runner, so these are
//! all no-ops (mirroring the JSC backend's `init.rs`).
#![allow(non_snake_case)]

use crate::Platform;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicPtr, Ordering};

type RawEntropySource = unsafe extern "C" fn(*mut u8, usize) -> bool;

unsafe extern "C" {
  fn v8__Platform__CustomPlatform__BASE__PostTask(
    context: *mut c_void,
    isolate: *mut c_void,
    task: *mut c_void,
  );
  fn v8__Platform__CustomPlatform__BASE__DROP(context: *mut c_void);
}

struct QuickJsPlatform {
  custom_context: *mut c_void,
}

struct QuickJsForegroundTask {
  isolate: *mut crate::RealIsolate,
}

static ENTROPY_SOURCE: AtomicPtr<std::ffi::c_void> =
  AtomicPtr::new(ptr::null_mut());
static RANDOM_SEED: AtomicI64 = AtomicI64::new(0);
static CURRENT_PLATFORM: AtomicPtr<std::ffi::c_void> =
  AtomicPtr::new(ptr::null_mut());

pub(crate) struct V8MathRandom {
  state0: u64,
  state1: u64,
  cache: [f64; 64],
  index: usize,
}

impl V8MathRandom {
  pub(crate) fn new(seed: u64) -> Self {
    Self {
      state0: murmur_hash3(seed),
      state1: murmur_hash3(!seed),
      cache: [0.0; 64],
      index: 0,
    }
  }

  pub(crate) fn next(&mut self) -> f64 {
    if self.index == 0 {
      for value in &mut self.cache {
        let mut state1 = self.state0;
        let state0 = self.state1;
        self.state0 = state0;
        state1 ^= state1 << 23;
        state1 ^= state1 >> 17;
        state1 ^= state0;
        state1 ^= state0 >> 26;
        self.state1 = state1;
        *value =
          ((state0.wrapping_add(state1)) >> 11) as f64 / ((1u64 << 53) as f64);
      }
      self.index = self.cache.len();
    }
    self.index -= 1;
    self.cache[self.index]
  }
}

fn murmur_hash3(mut value: u64) -> u64 {
  value ^= value >> 33;
  value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
  value ^= value >> 33;
  value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
  value ^= value >> 33;
  value
}

pub(crate) fn random_seed() -> Option<u64> {
  let seed = RANDOM_SEED.load(Ordering::Relaxed);
  (seed != 0).then_some(seed as u64)
}

pub(crate) fn has_entropy_source() -> bool {
  !ENTROPY_SOURCE.load(Ordering::SeqCst).is_null()
}

pub(crate) fn fill_entropy(buf: &mut [u8]) -> bool {
  let callback = ENTROPY_SOURCE.load(Ordering::SeqCst);
  if callback.is_null() {
    return false;
  }
  let callback: RawEntropySource = unsafe { std::mem::transmute(callback) };
  unsafe { callback(buf.as_mut_ptr(), buf.len()) }
}

pub(crate) fn post_foreground_task() {
  post_foreground_task_for(super::core::current_iso());
}

pub(crate) fn post_foreground_task_for(isolate: *mut crate::RealIsolate) {
  let platform = CURRENT_PLATFORM.load(Ordering::SeqCst);
  if platform.is_null() || isolate.is_null() {
    return;
  }
  let platform = platform as *mut QuickJsPlatform;
  let context = unsafe { (*platform).custom_context };
  if context.is_null() {
    return;
  }
  let task = Box::into_raw(Box::new(QuickJsForegroundTask { isolate }));
  unsafe {
    v8__Platform__CustomPlatform__BASE__PostTask(
      context,
      isolate as *mut c_void,
      task as *mut c_void,
    );
  }
}

pub(crate) unsafe fn run_foreground_task(task: *mut c_void) {
  let Some(task) = (unsafe { (task as *mut QuickJsForegroundTask).as_ref() })
  else {
    return;
  };
  super::core::drain_atomics_waiters(task.isolate);
}

pub(crate) unsafe fn delete_foreground_task(task: *mut c_void) {
  if !task.is_null() {
    unsafe { drop(Box::from_raw(task as *mut QuickJsForegroundTask)) };
  }
}

fn new_platform(custom_context: *mut c_void) -> *mut Platform {
  Box::into_raw(Box::new(QuickJsPlatform { custom_context })) as *mut Platform
}

unsafe fn drop_platform(platform: *mut Platform) {
  if platform.is_null() {
    return;
  }
  if CURRENT_PLATFORM.load(Ordering::SeqCst) == platform as *mut c_void {
    CURRENT_PLATFORM.store(ptr::null_mut(), Ordering::SeqCst);
  }
  let platform = unsafe { Box::from_raw(platform as *mut QuickJsPlatform) };
  if !platform.custom_context.is_null() {
    unsafe {
      v8__Platform__CustomPlatform__BASE__DROP(platform.custom_context)
    };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(platform: *mut Platform) {
  CURRENT_PLATFORM.store(platform as *mut c_void, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
  true
}

/// WebAssembly runs on the WAMR interpreter here, which detects out-of-bounds
/// accesses with explicit bounds checks instead of guard pages, so there is no
/// trap handler to install. Report failure, which is what V8 does on a build
/// without trap-handler support.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__EnableWebAssemblyTrapHandler(
  _use_v8_signal_handler: bool,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {
  CURRENT_PLATFORM.store(ptr::null_mut(), Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
  // Report the V8 version string our vendored rusty_v8 surface was generated
  // against (`v8::VERSION_STRING`), so `V8::get_version()` round-trips exactly.
  // This is a compat shim emulating V8; downstream code (e.g. Deno) compares
  // against the V8 version, not QuickJS's.
  use std::sync::OnceLock;
  static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
  VERSION
    .get_or_init(|| std::ffi::CString::new(crate::VERSION_STRING).unwrap())
    .as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
  argc: *mut c_int,
  argv: *mut *mut c_char,
  _usage: *const c_char,
) {
  if argc.is_null() || argv.is_null() {
    return;
  }

  unsafe {
    let len = *argc;
    if len <= 1 {
      return;
    }

    let mut write = 1;
    for read in 1..len {
      let arg = *argv.add(read as usize);
      if arg.is_null() {
        continue;
      }
      let arg = std::ffi::CStr::from_ptr(arg).to_string_lossy();
      if consume_v8_flag(&arg) {
        continue;
      }
      *argv.add(write as usize) = *argv.add(read as usize);
      write += 1;
    }
    *argc = write;
  }
}

#[derive(Debug, PartialEq, Eq)]
enum V8Flag {
  ForceStrict(bool),
  StackSize(usize),
  MaxOldSpaceSize(usize),
  RandomSeed(i32),
  Help,
  Noop,
}

fn parse_v8_flag(flag: &str) -> Option<V8Flag> {
  let flag = flag.trim_start_matches('-');
  let (name, value) = flag
    .split_once('=')
    .map_or((flag, None), |(name, value)| (name, Some(value)));
  let name = name.replace('-', "_");

  match name.as_str() {
    "use_strict" if value.is_none() => Some(V8Flag::ForceStrict(true)),
    "no_use_strict" if value.is_none() => Some(V8Flag::ForceStrict(false)),
    "stack_size" => value
      .and_then(|value| value.parse::<usize>().ok())
      .and_then(|kib| kib.checked_mul(1024))
      .map(V8Flag::StackSize),
    "external_memory_max_reasonable_size" => value
      .and_then(|value| value.parse::<usize>().ok())
      .map(|_| V8Flag::Noop),
    "max_old_space_size" => value
      .and_then(|value| value.parse::<usize>().ok())
      .and_then(|mib| mib.checked_mul(1024 * 1024))
      .map(V8Flag::MaxOldSpaceSize),
    "random_seed" => value
      .and_then(|value| value.parse::<i32>().ok())
      .map(V8Flag::RandomSeed),
    "inspector_live_edit" | "no_inspector_live_edit" if value.is_none() => {
      Some(V8Flag::Noop)
    }
    "gc_interval" => value
      .and_then(|value| value.parse::<usize>().ok())
      .map(|_| V8Flag::Noop),
    "allow_natives_syntax" | "expose_gc" | "jitless" | "trace_gc"
      if value.is_none() =>
    {
      Some(V8Flag::Noop)
    }
    "help" if value.is_none() => Some(V8Flag::Help),
    "log_colour" | "log_color" => Some(V8Flag::Noop),
    _ => None,
  }
}

fn consume_v8_flag(flag: &str) -> bool {
  match parse_v8_flag(flag) {
    Some(V8Flag::ForceStrict(true)) => {
      crate::quickjs::core::FORCE_STRICT
        .store(true, std::sync::atomic::Ordering::Relaxed);
      true
    }
    Some(V8Flag::ForceStrict(false)) => {
      crate::quickjs::core::FORCE_STRICT
        .store(false, std::sync::atomic::Ordering::Relaxed);
      true
    }
    Some(V8Flag::StackSize(bytes)) => {
      crate::quickjs::core::MAX_STACK_SIZE
        .store(bytes, std::sync::atomic::Ordering::Relaxed);
      true
    }
    Some(V8Flag::MaxOldSpaceSize(bytes)) => {
      crate::quickjs::core::MAX_HEAP_SIZE
        .store(bytes, std::sync::atomic::Ordering::Relaxed);
      true
    }
    Some(V8Flag::RandomSeed(seed)) => {
      RANDOM_SEED.store(seed as i64, Ordering::Relaxed);
      true
    }
    Some(V8Flag::Help) => {
      eprintln!(
        "V8 compatibility options:\nOptions:\n  --expose-gc (expose gc function)\n  --trace-gc (trace garbage collection)"
      );
      true
    }
    Some(V8Flag::Noop) => true,
    None => false,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(flags: *const u8, length: usize) {
  // Apply the same compatibility allowlist as the command-line parser. V8
  // normalizes `-`/`_` in flag names, so accept both spellings.
  if flags.is_null() || length == 0 {
    return;
  }
  let bytes = unsafe { std::slice::from_raw_parts(flags, length) };
  let Ok(text) = std::str::from_utf8(bytes) else {
    return;
  };
  for tok in text.split_whitespace() {
    consume_v8_flag(tok);
  }
}

#[cfg(test)]
mod tests {
  use super::{V8Flag, V8MathRandom, parse_v8_flag};

  #[test]
  fn parses_deno_v8_flags() {
    assert_eq!(
      parse_v8_flag("--stack-size=1024"),
      Some(V8Flag::StackSize(1024 * 1024))
    );
    assert_eq!(parse_v8_flag("--inspector-live-edit"), Some(V8Flag::Noop));
    assert_eq!(
      parse_v8_flag("--external-memory-max-reasonable-size=0"),
      Some(V8Flag::Noop)
    );
    assert_eq!(
      parse_v8_flag("--max-old-space-size=3072"),
      Some(V8Flag::MaxOldSpaceSize(3072 * 1024 * 1024))
    );
    assert_eq!(parse_v8_flag("--expose-gc"), Some(V8Flag::Noop));
    assert_eq!(parse_v8_flag("--allow-natives-syntax"), Some(V8Flag::Noop));
    assert_eq!(parse_v8_flag("--trace-gc"), Some(V8Flag::Noop));
    assert_eq!(parse_v8_flag("--jitless"), Some(V8Flag::Noop));
    assert_eq!(parse_v8_flag("--gc-interval=100"), Some(V8Flag::Noop));
    assert_eq!(
      parse_v8_flag("--random-seed=100"),
      Some(V8Flag::RandomSeed(100))
    );
    assert_eq!(parse_v8_flag("--help"), Some(V8Flag::Help));
  }

  #[test]
  fn rejects_unknown_or_malformed_v8_flags() {
    assert_eq!(parse_v8_flag("--stack-size=invalid"), None);
    assert_eq!(parse_v8_flag("--external-memory-max-reasonable-size"), None);
    assert_eq!(parse_v8_flag("--random-seed=invalid"), None);
    assert_eq!(parse_v8_flag("--definitely-not-a-v8-flag"), None);
  }

  #[test]
  fn seeded_math_random_matches_v8() {
    let mut random = V8MathRandom::new(100);
    let actual = std::array::from_fn::<_, 10, _>(|_| random.next());
    assert_eq!(
      actual,
      [
        0.832073805523701,
        0.7559025334996602,
        0.050689921012231465,
        0.5220240009004169,
        0.7277778086273778,
        0.06355390914564352,
        0.45059228063692036,
        0.12860342649349144,
        0.9774789444449407,
        0.8194241558700434,
      ]
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource(callback: *const std::ffi::c_void) {
  ENTROPY_SOURCE.store(callback.cast_mut(), Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewCustomPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
  _unprotected: bool,
  context: *mut std::ffi::c_void,
) -> *mut Platform {
  new_platform(context)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewUnprotectedDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop(
  _platform: *mut Platform,
  isolate: *mut std::ffi::c_void,
  _wait_for_work: bool,
) -> bool {
  let isolate = isolate as *mut crate::RealIsolate;
  if isolate.is_null() {
    return false;
  }
  let st = super::core::iso_state(isolate);
  if st.rt.is_null() {
    return false;
  }
  let resolved_waiters = super::core::drain_atomics_waiters(isolate);
  unsafe {
    let mut pctx = std::ptr::null_mut();
    resolved_waiters
      | (super::quickjs_sys::JS_ExecutePendingJob(st.rt, &mut pctx) > 0)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
  _platform: *mut Platform,
  _isolate: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE(this: *mut Platform) {
  unsafe { drop_platform(this) };
}

#[repr(C)]
struct PlatformSharedRepr {
  platform: *mut std::ffi::c_void,
  refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Platform>,
) -> SharedPtrBase<Platform> {
  let raw = unique_ptr.into_raw() as *mut std::ffi::c_void;
  let repr = if raw.is_null() {
    PlatformSharedRepr {
      platform: ptr::null_mut(),
      refcount: ptr::null_mut(),
    }
  } else {
    PlatformSharedRepr {
      platform: raw,
      refcount: Box::into_raw(Box::new(1usize)),
    }
  };
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(repr)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
  ptr: *const SharedPtrBase<Platform>,
) -> *mut Platform {
  if ptr.is_null() {
    return ptr::null_mut();
  }
  let repr = ptr as *const PlatformSharedRepr;
  unsafe { (*repr).platform as *mut Platform }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__COPY(
  ptr: *const SharedPtrBase<Platform>,
) -> SharedPtrBase<Platform> {
  if ptr.is_null() {
    return SharedPtrBase::default();
  }
  let repr = ptr as *const PlatformSharedRepr;
  let (platform, refcount) = unsafe { ((*repr).platform, (*repr).refcount) };
  if !refcount.is_null() {
    unsafe { *refcount += 1 };
  }
  let copy = PlatformSharedRepr { platform, refcount };
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(copy)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(
  ptr: *mut SharedPtrBase<Platform>,
) {
  if ptr.is_null() {
    return;
  }
  let repr = ptr as *mut PlatformSharedRepr;
  unsafe {
    let refcount = (*repr).refcount;
    if !refcount.is_null() {
      *refcount -= 1;
      if *refcount == 0 {
        drop(Box::from_raw(refcount));
        drop_platform((*repr).platform as *mut Platform);
      }
    }
    (*repr).platform = ptr::null_mut();
    (*repr).refcount = ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__use_count(
  ptr: *const SharedPtrBase<Platform>,
) -> long {
  if ptr.is_null() {
    return 0;
  }
  let repr = ptr as *const PlatformSharedRepr;
  let refcount = unsafe { (*repr).refcount };
  if refcount.is_null() {
    0
  } else {
    unsafe { *refcount as long }
  }
}
