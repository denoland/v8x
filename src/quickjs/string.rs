//! QuickJS-backed String shims (family: "string").
//!
//! Construction, inspection, writing, external strings, and PrimitiveArray.
//!
//! QuickJS-ng stores strings internally as either Latin-1 or UTF-16, but its C
//! ABI hands them out as UTF-8 (`JS_ToCStringLen`). For the v8 surface — which
//! is fundamentally UTF-16 (offsets/lengths in code units) — we transcode the
//! UTF-8 bytes to UTF-16 once per call and operate on that. Refcount rules: any
//! `JSValue` a QuickJS function RETURNS is owned (+1) and goes through
//! `intern`/`JS_FreeValue`; borrowed values we want to keep go through
//! `intern_dup`.
#![allow(non_snake_case)]

use super::core::{
  adjust_external_memory, adjust_external_string_memory, current_ctx,
  current_iso, intern, iso_state, jsval_of,
};
use super::quickjs_sys::*;
use crate::isolate::RealIsolate;
use crate::string::{Encoding, ExternalStringResourceBase, OneByteConst};
use crate::support::{char, int, size_t};
use crate::{Primitive, PrimitiveArray, String as V8String};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::OnceLock;

const K_NULL_TERMINATE: int =
  crate::binding::v8__String__WriteFlags__kNullTerminate as int;
const K_MAX_STRING_LENGTH: usize =
  crate::binding::v8__String__kMaxLength as usize;

fn locale_compare(
  left: &str,
  right: &str,
  numeric: bool,
) -> std::cmp::Ordering {
  static DEFAULT_COLLATOR: OnceLock<
    Option<icu_collator::CollatorBorrowed<'static>>,
  > = OnceLock::new();
  static NUMERIC_COLLATOR: OnceLock<
    Option<icu_collator::CollatorBorrowed<'static>>,
  > = OnceLock::new();
  let collator = if numeric {
    NUMERIC_COLLATOR.get_or_init(|| {
      let mut preferences: icu_collator::CollatorPreferences =
        icu_locale::locale!("en-US").into();
      preferences.numeric_ordering =
        Some(icu_collator::preferences::CollationNumericOrdering::True);
      icu_collator::Collator::try_new(preferences, Default::default()).ok()
    })
  } else {
    DEFAULT_COLLATOR.get_or_init(|| {
      icu_collator::Collator::try_new(
        icu_locale::locale!("en-US").into(),
        Default::default(),
      )
      .ok()
    })
  };
  collator
    .as_ref()
    .map(|collator| collator.compare(left, right))
    .unwrap_or_else(|| left.cmp(right))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_locale_compare_utf32(
  left: *const u32,
  left_len: c_int,
  right: *const u32,
  right_len: c_int,
  numeric: c_int,
) -> c_int {
  if left_len < 0
    || right_len < 0
    || (left.is_null() && left_len != 0)
    || (right.is_null() && right_len != 0)
  {
    return 0;
  }
  let left = if left_len == 0 {
    &[]
  } else {
    unsafe { std::slice::from_raw_parts(left, left_len as usize) }
  };
  let right = if right_len == 0 {
    &[]
  } else {
    unsafe { std::slice::from_raw_parts(right, right_len as usize) }
  };
  let left: std::string::String = left
    .iter()
    .filter_map(|&c| std::char::from_u32(c))
    .collect();
  let right: std::string::String = right
    .iter()
    .filter_map(|&c| std::char::from_u32(c))
    .collect();
  match locale_compare(&left, &right, numeric != 0) {
    std::cmp::Ordering::Less => -1,
    std::cmp::Ordering::Equal => 0,
    std::cmp::Ordering::Greater => 1,
  }
}

#[repr(C)]
struct ExternalOneByteMeta {
  bytes: Box<[u8]>,
}

enum ExternalMeta {
  OneByte(Box<ExternalOneByteMeta>),
  TwoByte,
}

thread_local! {
  static EXTERNAL_STRINGS: RefCell<HashMap<usize, ExternalMeta>> = RefCell::new(HashMap::new());
}

fn remember_external_onebyte(handle: *const V8String, bytes: Box<[u8]>) {
  if handle.is_null() {
    return;
  }
  EXTERNAL_STRINGS.with(|m| {
    m.borrow_mut().insert(
      handle as usize,
      ExternalMeta::OneByte(Box::new(ExternalOneByteMeta { bytes })),
    );
  });
}

fn remember_external_twobyte(handle: *const V8String) {
  if handle.is_null() {
    return;
  }
  EXTERNAL_STRINGS.with(|m| {
    m.borrow_mut()
      .insert(handle as usize, ExternalMeta::TwoByte);
  });
}

#[inline]
fn ctx_for(isolate: *mut RealIsolate) -> *mut JSContext {
  if !isolate.is_null() {
    super::core::iso_state(isolate)
      .contexts
      .last()
      .copied()
      .unwrap_or(super::core::iso_state(isolate).ctx)
  } else {
    current_ctx()
  }
}

#[inline]
fn account_external_string_memory(isolate: *mut RealIsolate, bytes: usize) {
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if isolate.is_null() || bytes == 0 {
    return;
  }
  let bytes = bytes.min(i64::MAX as usize) as i64;
  let st = iso_state(isolate);
  adjust_external_memory(st, bytes);
  adjust_external_string_memory(st, bytes);
}

#[inline]
fn handle_to_utf16(ctx: *mut JSContext, this: *const V8String) -> Vec<u16> {
  if ctx.is_null() || this.is_null() {
    return Vec::new();
  }
  let v = jsval_of(this);
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    return Vec::new();
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let out: Vec<u16> = std::string::String::from_utf8_lossy(bytes)
    .encode_utf16()
    .collect();
  unsafe { JS_FreeCString(ctx, cstr) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(
  isolate: *mut RealIsolate,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<V8String>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromTwoByte(
  isolate: *mut RealIsolate,
  data: *const u16,
  _new_type: int,
  length: int,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let len = if length < 0 { 0 } else { length as usize };
  if len > K_MAX_STRING_LENGTH {
    return ptr::null();
  }

  let utf8: std::string::String = if data.is_null() || len == 0 {
    std::string::String::new()
  } else {
    let units = unsafe { std::slice::from_raw_parts(data, len) };
    std::string::String::from_utf16_lossy(units)
  };
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let h = intern::<V8String>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsOneByte(_this: *const V8String) -> bool {
  v8__String__ContainsOnlyOneByte(_this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteConst(
  isolate: *mut RealIsolate,
  onebyte_const: *const OneByteConst,
) -> *const V8String {
  if onebyte_const.is_null() {
    return ptr::null();
  }
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let s: &str = unsafe { (*onebyte_const).as_str() };
  let bytes = s.as_bytes();
  let v = unsafe { JS_NewStringLen(ctx, s.as_ptr() as *const c_char, s.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let handle = intern::<V8String>(v);
  remember_external_onebyte(handle, bytes.to_vec().into_boxed_slice());
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteStatic(
  isolate: *mut RealIsolate,
  buffer: *const char,
  length: int,
) -> *const V8String {
  if buffer.is_null() || length < 0 {
    return ptr::null();
  }
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let bytes =
    unsafe { std::slice::from_raw_parts(buffer as *const u8, length as usize) };
  let utf8: std::string::String = bytes
    .iter()
    .map(|&b| b as u8 as ::std::primitive::char)
    .collect();
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let handle = intern::<V8String>(v);
  remember_external_onebyte(handle, bytes.to_vec().into_boxed_slice());
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByte(
  isolate: *mut RealIsolate,
  buffer: *mut char,
  length: usize,
  free: unsafe extern "C" fn(*mut char, usize),
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() || buffer.is_null() {
    if !buffer.is_null() {
      unsafe { free(buffer, length) };
    }
    return ptr::null();
  }

  let bytes =
    unsafe { std::slice::from_raw_parts(buffer as *const u8, length) };
  let owned = bytes.to_vec().into_boxed_slice();
  let utf8: std::string::String =
    owned.iter().map(|&b| b as ::std::primitive::char).collect();
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };

  unsafe { free(buffer, length) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  account_external_string_memory(isolate, length);
  let handle = intern::<V8String>(v);
  remember_external_onebyte(handle, owned);
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByte(
  isolate: *mut RealIsolate,
  buffer: *mut u16,
  length: usize,
  free: unsafe extern "C" fn(*mut u16, usize),
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() || buffer.is_null() {
    if !buffer.is_null() {
      unsafe { free(buffer, length) };
    }
    return ptr::null();
  }
  let units = unsafe { std::slice::from_raw_parts(buffer, length) };
  let utf8 = std::string::String::from_utf16_lossy(units);
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
  unsafe { free(buffer, length) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  account_external_string_memory(isolate, length.saturating_mul(2));
  let handle = intern::<V8String>(v);
  remember_external_twobyte(handle);
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResourceBase(
  this: *const V8String,
  encoding: *mut Encoding,
) -> *mut ExternalStringResourceBase {
  let mut out = ptr::null_mut();
  let mut enc = Encoding::Unknown;
  EXTERNAL_STRINGS.with(|m| {
    if let Some(meta) = m.borrow().get(&(this as usize)) {
      match meta {
        ExternalMeta::OneByte(resource) => {
          enc = Encoding::OneByte;
          out = (&**resource as *const ExternalOneByteMeta)
            as *mut ExternalStringResourceBase;
        }
        ExternalMeta::TwoByte => enc = Encoding::TwoByte,
      }
    }
  });
  if !encoding.is_null() {
    unsafe { *encoding = enc };
  }
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
  this: *const V8String,
  isolate: *mut RealIsolate,
) -> int {
  let ctx = ctx_for(isolate);
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let mut len: usize = 0;

  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    return 0;
  }
  unsafe { JS_FreeCString(ctx, cstr) };
  len as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ContainsOnlyOneByte(
  this: *const V8String,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return true;
  }

  handle_to_utf16(ctx, this).iter().all(|&c| c <= 0xFF)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Write_v2(
  this: *const V8String,
  isolate: *mut RealIsolate,
  offset: u32,
  length: u32,
  buffer: *mut u16,
  flags: int,
) {
  if this.is_null() || buffer.is_null() {
    return;
  }
  let ctx = ctx_for(isolate);
  let units = handle_to_utf16(ctx, this);
  let total = units.len();
  let start = offset as usize;
  let mut n = 0usize;
  if start < total {
    n = (total - start).min(length as usize);
    unsafe { ptr::copy_nonoverlapping(units.as_ptr().add(start), buffer, n) };
  }
  if (flags & K_NULL_TERMINATE) != 0 && (n as u32) < length {
    unsafe { *buffer.add(n) = 0 };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteOneByte_v2(
  this: *const V8String,
  isolate: *mut RealIsolate,
  offset: u32,
  length: u32,
  buffer: *mut u8,
  flags: int,
) {
  if this.is_null() || buffer.is_null() {
    return;
  }
  let ctx = ctx_for(isolate);
  let units = handle_to_utf16(ctx, this);
  let total = units.len();
  let start = offset as usize;
  let mut n = 0usize;
  if start < total {
    n = (total - start).min(length as usize);
    for i in 0..n {
      unsafe { *buffer.add(i) = units[start + i] as u8 };
    }
  }
  if (flags & K_NULL_TERMINATE) != 0 && (n as u32) < length {
    unsafe { *buffer.add(n) = 0 };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteUtf8_v2(
  this: *const V8String,
  isolate: *mut RealIsolate,
  buffer: *mut char,
  capacity: size_t,
  flags: int,
  processed_characters_return: *mut size_t,
) -> int {
  let set_processed = |n: size_t| {
    if !processed_characters_return.is_null() {
      unsafe { *processed_characters_return = n };
    }
  };
  if this.is_null() || buffer.is_null() {
    set_processed(0);
    return 0;
  }
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    set_processed(0);
    return 0;
  }
  let v = jsval_of(this);
  let mut len: usize = 0;
  // QuickJS hands back WTF-8: a valid surrogate PAIR is combined into 4-byte
  // UTF-8, but a LONE surrogate is emitted as its 3-byte CESU-8 form
  // (`ed a0-bf 80-bf` = U+D800..U+DFFF). v8 instead replaces lone surrogates
  // with U+FFFD (USVString semantics, which deno's encodeInto relies on), and
  // reports `processed` as UTF-16 code units consumed. Re-walk the WTF-8 and
  // re-emit, replacing lone surrogates and stopping on the buffer boundary.
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    set_processed(0);
    return 0;
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let out =
    unsafe { std::slice::from_raw_parts_mut(buffer as *mut u8, capacity) };

  let mut i = 0usize; // input byte cursor
  let mut w = 0usize; // output byte cursor
  let mut units: usize = 0; // UTF-16 code units consumed
  while i < bytes.len() {
    let b0 = bytes[i];
    // Decode one WTF-8 scalar: (output bytes, input byte length, utf16 units).
    let (enc, ilen, u16): ([u8; 4], usize, usize) = if b0 < 0x80 {
      ([b0, 0, 0, 0], 1, 1)
    } else if b0 >= 0xC0 && b0 < 0xE0 && i + 1 < bytes.len() {
      ([b0, bytes[i + 1], 0, 0], 2, 1)
    } else if b0 >= 0xE0 && b0 < 0xF0 && i + 2 < bytes.len() {
      let cp = (((b0 & 0x0F) as u32) << 12)
        | (((bytes[i + 1] & 0x3F) as u32) << 6)
        | ((bytes[i + 2] & 0x3F) as u32);
      if (0xD800..=0xDFFF).contains(&cp) {
        ([0xEF, 0xBF, 0xBD, 0], 3, 1) // lone surrogate -> U+FFFD
      } else {
        ([b0, bytes[i + 1], bytes[i + 2], 0], 3, 1)
      }
    } else if b0 >= 0xF0 && i + 3 < bytes.len() {
      ([b0, bytes[i + 1], bytes[i + 2], bytes[i + 3]], 4, 2)
    } else {
      ([0xEF, 0xBF, 0xBD, 0], 1, 1) // malformed -> U+FFFD
    };
    // Output byte length: replaced scalars (lone surrogate / malformed) emit
    // the 3-byte U+FFFD; everything else re-emits its original UTF-8 bytes.
    let replaced = enc[0] == 0xEF && enc[1] == 0xBF && enc[2] == 0xBD;
    let olen = if b0 < 0x80 {
      1
    } else if replaced {
      3
    } else {
      ilen
    };
    if w + olen > capacity {
      break;
    }
    out[w..w + olen].copy_from_slice(&enc[..olen]);
    w += olen;
    i += ilen;
    units += u16;
  }

  if (flags & K_NULL_TERMINATE) != 0 && w < capacity {
    out[w] = 0;
  }
  unsafe { JS_FreeCString(ctx, cstr) };

  set_processed(units as size_t);
  w as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__New(
  isolate: *mut RealIsolate,
  length: int,
) -> *const PrimitiveArray {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }

  let n = if length < 0 { 0 } else { length as u32 };
  for i in 0..n {
    unsafe { JS_SetPropertyUint32(ctx, arr, i, jsv_undefined()) };
  }
  intern::<PrimitiveArray>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Length(
  this: *const PrimitiveArray,
) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let obj = jsval_of(this);
  let len_val = unsafe { JS_GetPropertyStr(ctx, obj, c"length".as_ptr()) };
  if len_val.tag == JS_TAG_EXCEPTION {
    return 0;
  }
  let mut out: i32 = 0;
  unsafe { JS_ToInt32(ctx, &mut out, len_val) };
  unsafe { JS_FreeValue(ctx, len_val) };
  out as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Set(
  this: *const PrimitiveArray,
  isolate: *mut RealIsolate,
  index: int,
  item: *const Primitive,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return;
  }
  let obj = jsval_of(this);

  let val = if item.is_null() {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, jsval_of(item)) }
  };
  unsafe { JS_SetPropertyUint32(ctx, obj, index as u32, val) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Get(
  this: *const PrimitiveArray,
  isolate: *mut RealIsolate,
  index: int,
) -> *const Primitive {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  if this.is_null() || index < 0 {
    return intern::<Primitive>(jsv_undefined());
  }
  let obj = jsval_of(this);

  let v = unsafe { JS_GetPropertyUint32(ctx, obj, index as u32) };
  if v.tag == JS_TAG_EXCEPTION {
    return intern::<Primitive>(jsv_undefined());
  }
  intern::<Primitive>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
  isolate: *mut RealIsolate,
  data: *const c_char,
  _new_type: c_int,
  length: c_int,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let len = if length < 0 {
    if data.is_null() {
      0
    } else {
      unsafe { std::ffi::CStr::from_ptr(data) }.to_bytes().len()
    }
  } else {
    length as usize
  };
  if len > K_MAX_STRING_LENGTH {
    return ptr::null();
  }
  let bytes: &[u8] = if data.is_null() || len == 0 {
    &[]
  } else {
    unsafe { std::slice::from_raw_parts(data as *const u8, len) }
  };
  let utf8 = std::string::String::from_utf8_lossy(bytes);
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let h = intern::<V8String>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
  isolate: *mut RealIsolate,
  data: *const u8,
  _new_type: c_int,
  length: c_int,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let len = if length < 0 { 0 } else { length as usize };
  if len > K_MAX_STRING_LENGTH {
    return ptr::null();
  }

  let bytes: &[u8] = if data.is_null() || len == 0 {
    &[]
  } else {
    unsafe { std::slice::from_raw_parts(data, len) }
  };
  let utf8: std::string::String =
    bytes.iter().map(|&b| b as ::std::primitive::char).collect();
  let v =
    unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let h = intern::<V8String>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> c_int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    return 0;
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let count = std::string::String::from_utf8_lossy(bytes)
    .encode_utf16()
    .count();
  unsafe { JS_FreeCString(ctx, cstr) };
  count as c_int
}

/// Mirrors `v8::String::ValueView` from `v8-primitive.h`, because rusty_v8
/// v152.0.0 stopped calling the `data`/`length`/`is_one_byte` accessors and
/// now reads these fields straight out of the buffer it hands to `CONSTRUCT`:
/// a pointer, the data pointer, a `u32` length, then a `bool`. The trailing
/// word is V8 padding and this backend leaves it alone. Total size must stay
/// within `v8__String__ValueView_SIZE` (32).
#[repr(C)]
pub(super) struct ViewState {
  /// V8 keeps `Local<v8::String> flat_str_` here. QuickJS owns a transcoded
  /// copy instead, so the slot stays null.
  flat_str: *mut c_void,
  data: *mut c_void,
  len: u32,
  is_one_byte: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
  buf: *mut ViewState,
  isolate: *mut RealIsolate,
  string: *const V8String,
) {
  let ctx = ctx_for(isolate);
  let units: Vec<u16> = if ctx.is_null() || string.is_null() {
    Vec::new()
  } else {
    let v = jsval_of(string);
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
      Vec::new()
    } else {
      let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
      let s = std::string::String::from_utf8_lossy(bytes);
      let out: Vec<u16> = s.encode_utf16().collect();
      unsafe { JS_FreeCString(ctx, cstr) };
      out
    }
  };
  let is_one_byte = units.iter().all(|&u| u <= 0xFF);
  let len = units.len();
  let data = if is_one_byte {
    let bytes: Box<[u8]> = units.iter().map(|&u| u as u8).collect();
    Box::into_raw(bytes) as *mut c_void
  } else {
    Box::into_raw(units.into_boxed_slice()) as *mut c_void
  };
  unsafe {
    (*buf).flat_str = ptr::null_mut();
    (*buf).data = data;
    (*buf).len = len as u32;
    (*buf).is_one_byte = is_one_byte;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(this: *mut ViewState) {
  unsafe {
    let st = &mut *this;
    if !st.data.is_null() {
      let len = st.len as usize;
      if st.is_one_byte {
        let slice = ptr::slice_from_raw_parts_mut(st.data as *mut u8, len);
        drop(Box::from_raw(slice));
      } else {
        let slice = ptr::slice_from_raw_parts_mut(st.data as *mut u16, len);
        drop(Box::from_raw(slice));
      }
      st.data = ptr::null_mut();
      st.len = 0;
      st.is_one_byte = true;
    }
  }
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__data(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    (*(this as *const ExternalOneByteMeta)).bytes.as_ptr() as *const c_void
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ExternalOneByteStringResource__length(
  this: *const std::os::raw::c_void,
) -> usize {
  if this.is_null() {
    return 0;
  }
  unsafe { (&(*(this as *const ExternalOneByteMeta)).bytes).len() }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Concat(
  isolate: *mut RealIsolate,
  left: *const V8String,
  right: *const V8String,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() || left.is_null() || right.is_null() {
    return ptr::null();
  }
  let mut units = handle_to_utf16(ctx, left);
  units.extend(handle_to_utf16(ctx, right));
  let s = std::string::String::from_utf16_lossy(&units);
  let v = unsafe { JS_NewStringLen(ctx, s.as_ptr() as *const c_char, s.len()) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<V8String>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalOneByte(
  this: *const std::os::raw::c_void,
) -> bool {
  EXTERNAL_STRINGS.with(|m| {
    matches!(
      m.borrow().get(&(this as usize)),
      Some(ExternalMeta::OneByte(_))
    )
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsExternalTwoByte(
  this: *const std::os::raw::c_void,
) -> bool {
  EXTERNAL_STRINGS.with(|m| {
    matches!(
      m.borrow().get(&(this as usize)),
      Some(ExternalMeta::TwoByte)
    )
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByteStatic(
  isolate: *mut RealIsolate,
  buffer: *const u16,
  length: crate::support::int,
) -> *const V8String {
  if buffer.is_null() || length < 0 {
    return ptr::null();
  }
  let handle = v8__String__NewFromTwoByte(isolate, buffer, 0, length);
  remember_external_twobyte(handle);
  handle
}

#[cfg(test)]
mod tests {
  use super::locale_compare;
  use std::cmp::Ordering;

  #[test]
  fn default_locale_compare_matches_v8() {
    assert_eq!(
      locale_compare("assert.ts", "assert_equals.ts", false),
      Ordering::Greater
    );
    assert_eq!(locale_compare("a-b", "ab", false), Ordering::Less);
    assert_eq!(locale_compare("A", "a", false), Ordering::Greater);
    assert_eq!(locale_compare("\u{e4}", "z", false), Ordering::Less);
    assert_eq!(locale_compare("2", "10", false), Ordering::Greater);
    assert_eq!(locale_compare("a2", "a10", true), Ordering::Less);
  }
}

unsafe extern "C" {
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__StringEquals(
  this: *const V8String,
  that: *const V8String,
) -> bool {
  if this.is_null() || that.is_null() {
    return false;
  }
  if ptr::eq(this, that) {
    return true;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return false;
  }
  unsafe { JS_IsStrictEqual(ctx, jsval_of(this), jsval_of(that)) }
}

/// QuickJS interns strings as atoms, so the atom's string is the canonical
/// copy: equal inputs come back as one and the same string value.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__InternalizeString(
  this: *const V8String,
  isolate: *mut RealIsolate,
) -> *const V8String {
  let ctx = ctx_for(isolate);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let value = jsval_of(this);
  let atom = unsafe { JS_ValueToAtom(ctx, value) };
  if atom == 0 {
    unsafe { super::exception::clear_pending(ctx) };
    return super::core::intern_dup::<V8String>(ctx, value);
  }
  let internalized = unsafe { JS_AtomToString(ctx, atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if internalized.tag == JS_TAG_EXCEPTION {
    unsafe { super::exception::clear_pending(ctx) };
    return super::core::intern_dup::<V8String>(ctx, value);
  }
  intern::<V8String>(internalized)
}
