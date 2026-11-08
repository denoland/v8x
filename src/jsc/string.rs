#![allow(non_snake_case, unused)]

use crate::isolate::RealIsolate;
use crate::jsc::core::{
  adjust_external_memory, adjust_external_string_memory, ctx_of, current_ctx,
  current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::string::{Encoding, NewStringType};
use crate::support::{char, int, size_t};
use crate::{Primitive, PrimitiveArray, String as V8String, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::ptr;

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
unsafe fn make_string_from_utf16(
  ctx: JSContextRef,
  data: *const u16,
  len: usize,
) -> JSValueRef {
  unsafe {
    let s = JSStringCreateWithCharacters(data, len);
    let v = JSValueMakeString(ctx, s);
    JSStringRelease(s);
    v
  }
}

/// The context to create a string in. V8 creates strings on a bare
/// `HandleScope` too, so fall back to the isolate's base context when none is
/// entered.
fn creation_ctx(isolate: *mut RealIsolate) -> JSContextRef {
  let ctx = current_ctx();
  if !ctx.is_null() {
    return ctx;
  }
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  crate::jsc::core::ensure_ctx(isolate)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(
  isolate: *mut RealIsolate,
) -> *const V8String {
  let ctx = creation_ctx(isolate);
  unsafe {
    let s = JSStringCreateWithUTF8CString(c"".as_ptr());
    let v = JSValueMakeString(ctx, s);
    JSStringRelease(s);
    intern_ctx::<V8String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
  isolate: *mut RealIsolate,
  data: *const char,
  _new_type: NewStringType,
  length: int,
) -> *const V8String {
  let ctx = creation_ctx(isolate);
  if length < 0 || data.is_null() {
    return ptr::null();
  }
  let bytes =
    unsafe { std::slice::from_raw_parts(data as *const u8, length as usize) };

  let cow = std::string::String::from_utf8_lossy(bytes);
  let utf16: Vec<u16> = cow.encode_utf16().collect();
  unsafe {
    let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
    intern_ctx::<V8String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
  isolate: *mut RealIsolate,
  data: *const u8,
  _new_type: NewStringType,
  length: int,
) -> *const V8String {
  let ctx = creation_ctx(isolate);
  if length < 0 || data.is_null() {
    return ptr::null();
  }

  let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
  let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
  unsafe {
    let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
    intern_ctx::<V8String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromTwoByte(
  isolate: *mut RealIsolate,
  data: *const u16,
  _new_type: NewStringType,
  length: int,
) -> *const V8String {
  let ctx = creation_ctx(isolate);
  if length < 0 || data.is_null() {
    return ptr::null();
  }
  unsafe {
    let v = make_string_from_utf16(ctx, data, length as usize);
    intern_ctx::<V8String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsOneByte(this: *const V8String) -> bool {
  if this.is_null() {
    return true;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return true;
  }
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return true;
    }
    let len = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let mut one_byte = true;
    if !chars.is_null() {
      for i in 0..len {
        if *chars.add(i) > 0xFF {
          one_byte = false;
          break;
        }
      }
    }
    JSStringRelease(s);
    one_byte
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> int {
  if this.is_null() {
    return 0;
  }
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return 0;
    }
    let len = JSStringGetLength(s);
    JSStringRelease(s);
    len as int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
  this: *const V8String,
  isolate: *mut RealIsolate,
) -> int {
  if this.is_null() {
    return 0;
  }
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return 0;
    }

    let cap = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf: Vec<u8> = vec![0; cap];
    let written = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut char, cap);
    JSStringRelease(s);

    (written.saturating_sub(1)) as int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ContainsOnlyOneByte(
  this: *const V8String,
) -> bool {
  if this.is_null() {
    return true;
  }
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return true;
    }
    let len = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let mut only_one_byte = true;
    if !chars.is_null() {
      let slice = std::slice::from_raw_parts(chars, len);
      only_one_byte = slice.iter().all(|&c| c <= 0xFF);
    }
    JSStringRelease(s);
    only_one_byte
  }
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
  let null_terminate =
    crate::binding::v8__String__WriteFlags__kNullTerminate as int;
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return;
    }
    let total = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let start = offset as usize;
    let mut n = 0usize;
    if !chars.is_null() && start < total {
      let avail = total - start;
      n = avail.min(length as usize);
      ptr::copy_nonoverlapping(chars.add(start), buffer, n);
    }
    JSStringRelease(s);
    if (flags & null_terminate) != 0 && (n as u32) < length {
      *buffer.add(n) = 0;
    }
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
  let null_terminate =
    crate::binding::v8__String__WriteFlags__kNullTerminate as int;
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      return;
    }
    let total = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let start = offset as usize;
    let mut n = 0usize;
    if !chars.is_null() && start < total {
      let avail = total - start;
      n = avail.min(length as usize);
      for i in 0..n {
        *buffer.add(i) = (*chars.add(start + i)) as u8;
      }
    }
    JSStringRelease(s);
    if (flags & null_terminate) != 0 && (n as u32) < length {
      *buffer.add(n) = 0;
    }
  }
}

fn utf8_complete_prefix(s: &[u8], cap: usize) -> (usize, size_t) {
  let mut i = 0usize;
  let mut units: size_t = 0;
  while i < s.len() {
    let b = s[i];
    let seq = if b < 0x80 {
      1
    } else if b < 0xE0 {
      2
    } else if b < 0xF0 {
      3
    } else {
      4
    };
    if i + seq > cap {
      break;
    }
    units += if seq == 4 { 2 } else { 1 };
    i += seq;
  }
  (i, units)
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
  if this.is_null() || buffer.is_null() {
    if !processed_characters_return.is_null() {
      unsafe { *processed_characters_return = 0 };
    }
    return 0;
  }
  let ctx = current_ctx();
  unsafe {
    let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
    if s.is_null() {
      if !processed_characters_return.is_null() {
        *processed_characters_return = 0;
      }
      return 0;
    }
    let char_count = JSStringGetLength(s);

    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut scratch: Vec<u8> = vec![0; max];
    let written =
      JSStringGetUTF8CString(s, scratch.as_mut_ptr() as *mut char, max);
    JSStringRelease(s);

    let utf8_len = written.saturating_sub(1);

    let (copy, units) = if utf8_len <= capacity {
      (utf8_len, char_count)
    } else {
      utf8_complete_prefix(&scratch[..utf8_len], capacity)
    };
    if copy > 0 {
      ptr::copy_nonoverlapping(scratch.as_ptr(), buffer as *mut u8, copy);
    }

    if copy < capacity {
      *(buffer as *mut u8).add(copy) = 0;
    }
    if !processed_characters_return.is_null() {
      *processed_characters_return = units;
    }
    copy as int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteConst(
  isolate: *mut RealIsolate,
  onebyte_const: *const crate::string::OneByteConst,
) -> *const V8String {
  if onebyte_const.is_null() {
    return ptr::null();
  }
  let ctx = current_ctx();
  unsafe {
    let s: &str = (*onebyte_const).as_str();
    let bytes = s.as_bytes();
    let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
    let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
    let handle = intern_ctx::<V8String>(ctx, v);
    remember_external_onebyte(handle, bytes.to_vec().into_boxed_slice());
    handle
  }
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
  let ctx = current_ctx();
  unsafe {
    let bytes =
      std::slice::from_raw_parts(buffer as *const u8, length as usize);
    let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
    let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
    let handle = intern_ctx::<V8String>(ctx, v);
    remember_external_onebyte(handle, bytes.to_vec().into_boxed_slice());
    handle
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResourceBase(
  this: *const V8String,
  encoding: *mut Encoding,
) -> *mut crate::string::ExternalStringResourceBase {
  let mut out = ptr::null_mut();
  let mut enc = Encoding::Unknown;
  EXTERNAL_STRINGS.with(|m| {
    if let Some(meta) = m.borrow().get(&(this as usize)) {
      match meta {
        ExternalMeta::OneByte(resource) => {
          enc = Encoding::OneByte;
          out = (&**resource as *const ExternalOneByteMeta)
            as *mut crate::string::ExternalStringResourceBase;
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

/// Mirrors `v8::String::ValueView` from `v8-primitive.h`. rusty_v8 v152.0.0
/// stopped calling the `data`/`length`/`is_one_byte` accessors and now reads
/// these fields straight out of the buffer handed to `CONSTRUCT`: a pointer,
/// the data pointer, a `u32` length, then a `bool`. `onebyte_owned` occupies
/// the trailing word V8 uses as padding, keeping the whole thing inside
/// `v8__String__ValueView_SIZE` (32).
#[repr(C)]
struct ValueViewRepr {
  js_string: JSStringRef,
  data: *const c_void,
  len: u32,
  is_one_byte: bool,

  onebyte_owned: *mut u8,
}

impl ValueViewRepr {
  #[inline]
  fn length(&self) -> usize {
    self.len as usize
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
  buf: *mut crate::string::ValueView,
  isolate: *mut RealIsolate,
  string: *const V8String,
) {
  let repr = buf as *mut ValueViewRepr;
  let ctx = current_ctx();
  unsafe {
    if string.is_null() {
      (*repr) = ValueViewRepr {
        js_string: ptr::null_mut(),
        data: ptr::null(),
        len: 0 as u32,
        is_one_byte: true,
        onebyte_owned: ptr::null_mut(),
      };
      return;
    }
    let s = JSValueToStringCopy(ctx, jsval(string), ptr::null_mut());
    if s.is_null() {
      (*repr) = ValueViewRepr {
        js_string: ptr::null_mut(),
        data: ptr::null(),
        len: 0 as u32,
        is_one_byte: true,
        onebyte_owned: ptr::null_mut(),
      };
      return;
    }
    let len = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let only_one_byte = if chars.is_null() {
      true
    } else {
      std::slice::from_raw_parts(chars, len)
        .iter()
        .all(|&c| c <= 0xFF)
    };

    if only_one_byte {
      let mut owned: Vec<u8> = Vec::with_capacity(len);
      if !chars.is_null() {
        for i in 0..len {
          owned.push((*chars.add(i)) as u8);
        }
      } else {
        owned.resize(len, 0);
      }
      let boxed = owned.into_boxed_slice();
      let ptr = Box::into_raw(boxed) as *mut u8;
      (*repr) = ValueViewRepr {
        js_string: s,
        data: ptr as *const c_void,
        len: len as u32,
        is_one_byte: true,
        onebyte_owned: ptr,
      };
    } else {
      (*repr) = ValueViewRepr {
        js_string: s,
        data: chars as *const c_void,
        len: len as u32,
        is_one_byte: false,
        onebyte_owned: ptr::null_mut(),
      };
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(
  this: *mut crate::string::ValueView,
) {
  let repr = this as *mut ValueViewRepr;
  unsafe {
    if !(*repr).onebyte_owned.is_null() {
      let len = (*repr).length();
      let slice = ptr::slice_from_raw_parts_mut((*repr).onebyte_owned, len);
      drop(Box::from_raw(slice));
      (*repr).onebyte_owned = ptr::null_mut();
    }
    if !(*repr).js_string.is_null() {
      JSStringRelease((*repr).js_string);
      (*repr).js_string = ptr::null_mut();
    }
    (*repr).data = ptr::null();
    (*repr).len = 0;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__New(
  isolate: *mut RealIsolate,
  length: int,
) -> *const PrimitiveArray {
  let ctx = current_ctx();
  let n = if length < 0 { 0 } else { length as usize };
  unsafe {
    let undef = JSValueMakeUndefined(ctx);
    let args: Vec<JSValueRef> = vec![undef; n];
    let arr = JSObjectMakeArray(
      ctx,
      n,
      if n == 0 { ptr::null() } else { args.as_ptr() },
      ptr::null_mut(),
    );
    intern_ctx::<PrimitiveArray>(ctx, arr as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Length(
  this: *const PrimitiveArray,
) -> int {
  if this.is_null() {
    return 0;
  }
  let ctx = current_ctx();
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let name = JSStringCreateWithUTF8CString(c"length".as_ptr());
    let v = JSObjectGetProperty(ctx, obj, name, ptr::null_mut());
    JSStringRelease(name);
    let n = JSValueToNumber(ctx, v, ptr::null_mut());
    if n.is_nan() { 0 } else { n as int }
  }
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
  let ctx = current_ctx();
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let val = if item.is_null() {
      JSValueMakeUndefined(ctx)
    } else {
      jsval(item)
    };
    JSObjectSetPropertyAtIndex(ctx, obj, index as u32, val, ptr::null_mut());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Get(
  this: *const PrimitiveArray,
  isolate: *mut RealIsolate,
  index: int,
) -> *const Primitive {
  let ctx = current_ctx();
  if this.is_null() || index < 0 {
    return intern_ctx::<Primitive>(ctx, unsafe { JSValueMakeUndefined(ctx) });
  }
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let v = JSObjectGetPropertyAtIndex(ctx, obj, index as u32, ptr::null_mut());
    intern_ctx::<Primitive>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByte(
  isolate: *mut RealIsolate,
  buffer: *mut char,
  length: size_t,
  free: unsafe extern "C" fn(*mut char, size_t),
) -> *const V8String {
  let ctx = current_ctx();
  if buffer.is_null() {
    return ptr::null();
  }
  let bytes =
    unsafe { std::slice::from_raw_parts(buffer as *const u8, length as usize) };
  let owned = bytes.to_vec().into_boxed_slice();
  let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
  let v = unsafe { make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len()) };
  let out = intern_ctx::<V8String>(ctx, v);
  remember_external_onebyte(out, owned);

  account_external_string_memory(isolate, length as usize);
  unsafe { free(buffer, length) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByte(
  isolate: *mut RealIsolate,
  buffer: *mut u16,
  length: size_t,
  free: unsafe extern "C" fn(*mut u16, size_t),
) -> *const V8String {
  let ctx = current_ctx();
  if buffer.is_null() {
    return ptr::null();
  }
  let v = unsafe { make_string_from_utf16(ctx, buffer, length as usize) };
  let out = intern_ctx::<V8String>(ctx, v);
  remember_external_twobyte(out);
  account_external_string_memory(isolate, (length as usize).saturating_mul(2));
  unsafe { free(buffer, length) };
  out
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
  _isolate: *mut RealIsolate,
  left: *const V8String,
  right: *const V8String,
) -> *const V8String {
  if left.is_null() || right.is_null() {
    return ptr::null();
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let left_s = JSValueToStringCopy(ctx, jsval(left), ptr::null_mut());
    let right_s = JSValueToStringCopy(ctx, jsval(right), ptr::null_mut());
    if left_s.is_null() || right_s.is_null() {
      if !left_s.is_null() {
        JSStringRelease(left_s);
      }
      if !right_s.is_null() {
        JSStringRelease(right_s);
      }
      return ptr::null();
    }
    let left_len = JSStringGetLength(left_s);
    let right_len = JSStringGetLength(right_s);
    let mut units = Vec::with_capacity(left_len + right_len);
    let left_chars = JSStringGetCharactersPtr(left_s);
    let right_chars = JSStringGetCharactersPtr(right_s);
    if !left_chars.is_null() {
      units.extend_from_slice(std::slice::from_raw_parts(left_chars, left_len));
    }
    if !right_chars.is_null() {
      units
        .extend_from_slice(std::slice::from_raw_parts(right_chars, right_len));
    }
    JSStringRelease(left_s);
    JSStringRelease(right_s);
    let v = make_string_from_utf16(ctx, units.as_ptr(), units.len());
    intern_ctx::<V8String>(ctx, v)
  }
}

/// The UTF-16 code units of the string value `v`, or `None` if it cannot be
/// converted.
unsafe fn utf16_units(ctx: JSContextRef, v: JSValueRef) -> Option<Vec<u16>> {
  unsafe {
    let s = JSValueToStringCopy(ctx, v, ptr::null_mut());
    if s.is_null() {
      return None;
    }
    let len = JSStringGetLength(s);
    let chars = JSStringGetCharactersPtr(s);
    let units = if chars.is_null() || len == 0 {
      Vec::new()
    } else {
      std::slice::from_raw_parts(chars, len).to_vec()
    };
    JSStringRelease(s);
    Some(units)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__StringEquals(
  this: *const V8String,
  that: *const V8String,
) -> bool {
  if this.is_null() || that.is_null() {
    return this == that;
  }
  if this == that {
    return true;
  }
  let ctx = crate::jsc::core::ensure_ctx(current_iso());
  if ctx.is_null() {
    return false;
  }
  // Strict equality on two strings compares their contents.
  unsafe { JSValueIsStrictEqual(ctx, jsval(this), jsval(that)) }
}

/// JSC exposes no string table, so the isolate keeps its own: the first
/// string internalized with given contents becomes the canonical handle for
/// them, which makes equal internalized strings the same handle as in V8.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__InternalizeString(
  this: *const V8String,
  isolate: *mut RealIsolate,
) -> *const V8String {
  if this.is_null() {
    return ptr::null();
  }
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if isolate.is_null() {
    return this;
  }
  // Protect the canonical value against the base context, which lives until
  // `Isolate::Dispose` releases these first.
  let st = iso_state(isolate);
  if st.base_ctx.is_null() {
    st.base_ctx =
      unsafe { JSGlobalContextCreateInGroup(st.group, ptr::null_mut()) };
  }
  let base = st.base_ctx as JSContextRef;
  let Some(units) = (unsafe { utf16_units(base, jsval(this)) }) else {
    return this;
  };
  let v = *st.internalized.entry(units).or_insert_with(|| {
    let v = jsval(this);
    unsafe { JSValueProtect(base, v) };
    v
  });
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
  let handle =
    v8__String__NewFromTwoByte(isolate, buffer, NewStringType::Normal, length);
  remember_external_twobyte(handle);
  handle
}
