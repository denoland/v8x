//! Extra C-ABI symbols referenced by the full `deno` cli (beyond deno_core).
//! Kept in a standalone module so it never collides with the family shims.
#![allow(non_snake_case, unused)]

use crate::Name;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

// Process-wide default locale (BCP47 language tag). We just remember whatever
// `icu_set_default_locale` was last handed and report it back from
// `icu_get_default_locale`. Defaults to "en-US".
fn locale_store() -> &'static Mutex<String> {
  static LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
  LOCALE.get_or_init(|| Mutex::new("en-US".to_string()))
}

// Canonicalize a locale id to a BCP47-ish language tag the way ICU's
// `uloc_toLanguageTag` does for simple ids, e.g. "nb_NO" -> "nb-NO".
pub(crate) fn set_default_locale_str(s: &str) {
  let tag = s.replace('_', "-");
  if let Ok(mut g) = locale_store().lock() {
    *g = tag;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__IsSandboxEnabled() -> bool {
  false
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__CONSTRUCT(
  _buf: *mut c_void,
  _isolate: *mut c_void,
) {
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__DESTRUCT(
  _this: *mut c_void,
) {
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewRustAllocator(
  _handle: *mut c_void,
  _vtable: *const c_void,
) -> *mut c_void {
  std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_locale(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let guard = locale_store().lock();
  let loc: Vec<u8> = match guard {
    Ok(g) => g.as_bytes().to_vec(),
    Err(_) => b"en-US".to_vec(),
  };
  if output.is_null() || output_len == 0 {
    return loc.len();
  }
  let n = loc.len().min(output_len.saturating_sub(1));
  unsafe {
    std::ptr::copy_nonoverlapping(loc.as_ptr() as *const c_char, output, n);
    *output.add(n) = 0;
  }
  n
}

// The default time zone lives in ICU, which JavaScriptCore consults for
// local time: both the system framework and the vendored build use Apple's
// libicucore, whose C API is exported without version suffixes.
#[link(name = "icucore")]
unsafe extern "C" {
  fn ucal_getDefaultTimeZone(
    result: *mut u16,
    result_capacity: i32,
    ec: *mut i32,
  ) -> i32;
  fn ucal_setDefaultTimeZone(zone_id: *const u16, ec: *mut i32);
  fn ucal_getCanonicalTimeZoneID(
    id: *const u16,
    len: i32,
    result: *mut u16,
    result_capacity: i32,
    is_system_id: *mut i8,
    status: *mut i32,
  ) -> i32;
}

/// ICU's `U_FAILURE`: warnings are negative, errors positive.
fn icu_failed(status: i32) -> bool {
  status > 0
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_time_zone(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let mut buf = [0u16; 128];
  let mut status = 0;
  let n = unsafe {
    ucal_getDefaultTimeZone(buf.as_mut_ptr(), buf.len() as i32, &mut status)
  };
  if icu_failed(status) || n < 0 || output.is_null() {
    return 0;
  }
  let id = String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]);
  // Like ICU's `CheckedArrayByteSink`, write what fits and report that.
  let len = id.len().min(output_len);
  unsafe {
    std::ptr::copy_nonoverlapping(id.as_ptr() as *const c_char, output, len)
  };
  len
}

/// Installs `time_zone_id` as ICU's default if ICU recognizes it. Like
/// rusty_v8's ICU binding, ICU's "unknown" zone counts as unrecognized.
#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_time_zone(
  time_zone_id: *const c_char,
) -> bool {
  if time_zone_id.is_null() {
    return false;
  }
  let id = unsafe { std::ffi::CStr::from_ptr(time_zone_id) }.to_string_lossy();
  let mut units: Vec<u16> = id.encode_utf16().collect();
  let mut canonical = [0u16; 128];
  let mut is_system = 0i8;
  let mut status = 0;
  let n = unsafe {
    ucal_getCanonicalTimeZoneID(
      units.as_ptr(),
      units.len() as i32,
      canonical.as_mut_ptr(),
      canonical.len() as i32,
      &mut is_system,
      &mut status,
    )
  };
  if icu_failed(status) || n <= 0 {
    return false;
  }
  let unknown: Vec<u16> = "Etc/Unknown".encode_utf16().collect();
  if canonical[..(n as usize).min(canonical.len())] == unknown[..] {
    return false;
  }
  units.push(0);
  let mut status = 0;
  unsafe { ucal_setDefaultTimeZone(units.as_ptr(), &mut status) };
  !icu_failed(status)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback(
  isolate: *mut c_void,
  callback: crate::isolate::NearHeapLimitCallback,
  data: *mut c_void,
) {
  crate::jsc::terminate::set_heap_callback(
    isolate as *mut crate::RealIsolate,
    callback,
    data,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Name__GetIdentityHash(this: *const Name) -> c_int {
  let h = (this as usize as u64).wrapping_mul(0x9E3779B97F4A7C15);
  ((h >> 33) as u32 as i32) | 1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFatalErrorHandler(_that: *const c_void) {}

#[cfg(feature = "vendor_jsc")]
#[unsafe(no_mangle)]
pub extern "C" fn os_unfair_lock_lock_with_flags(
  lock: *mut std::ffi::c_void,
  _flags: u32,
) {
  unsafe extern "C" {
    fn os_unfair_lock_lock(lock: *mut std::ffi::c_void);
  }
  unsafe { os_unfair_lock_lock(lock) }
}
