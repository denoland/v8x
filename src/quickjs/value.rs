//! QuickJS-ng-backed `v8::Value` shims — family "value".
//!
//! Implements the v8 Value predicates (`Is*`), conversions (`To*`,
//! `NumberValue`, `Int32Value`), and `StrictEquals` against the QuickJS-ng
//! runtime. Ported from the deno PR's QuickJS logic
//! (reference/qjs_v8_compat/src/value.rs) and structured to mirror the JSC
//! backend's `src/value.rs` C-ABI shape — but every JSC call is replaced
//! by the equivalent QuickJS-ng call.
//!
//! Refcount discipline:
//!  * Predicates (`Is*`, `StrictEquals`) only *read* a borrowed JSValue (via
//!    `jsval_of`) and never mint a new handle, so no dup/free is required.
//!  * Conversions that return a NEW handle route the OWNED (+1) JSValue
//!    produced by a QuickJS `JS_*` call through `intern`, transferring
//!    ownership into the current handle scope. For values that are merely a
//!    borrowed pass-through of `this`, we `intern_dup` (JS_DupValue) so the
//!    arena owns its own refcount.
//!  * Any temporary JSValue we create and do NOT keep (globals, ctor lookups,
//!    `Object.prototype.toString` machinery, call results) is released with
//!    `JS_FreeValue`.

#![allow(non_snake_case)]

use crate::quickjs::core::{
  ctx_of, current_ctx, intern, intern_dup, is_interned_handle, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::support::Maybe;
use crate::{
  BigInt, Boolean, Context, Int32, Integer, Number, Object, RealIsolate,
  String as V8String, Uint32, Value,
};
use std::os::raw::c_char;
use std::ptr;

unsafe extern "C" {

  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;

  fn JS_ToObject(ctx: *mut JSContext, val: JSValue) -> JSValue;

  fn JS_IsInstanceOf(
    ctx: *mut JSContext,
    val: JSValue,
    obj: JSValue,
  ) -> std::os::raw::c_int;

  fn JS_GetTypedArrayType(obj: JSValue) -> std::os::raw::c_int;

  fn JS_IsError(val: JSValue) -> bool;
}

const JS_TYPED_ARRAY_UINT8C: i32 = 0;
const JS_TYPED_ARRAY_INT8: i32 = 1;
const JS_TYPED_ARRAY_UINT8: i32 = 2;
const JS_TYPED_ARRAY_INT16: i32 = 3;
const JS_TYPED_ARRAY_UINT16: i32 = 4;
const JS_TYPED_ARRAY_INT32: i32 = 5;
const JS_TYPED_ARRAY_UINT32: i32 = 6;
const JS_TYPED_ARRAY_BIG_INT64: i32 = 7;
const JS_TYPED_ARRAY_BIG_UINT64: i32 = 8;
const JS_TYPED_ARRAY_FLOAT16: i32 = 9;
const JS_TYPED_ARRAY_FLOAT32: i32 = 10;
const JS_TYPED_ARRAY_FLOAT64: i32 = 11;
const QUICKJS_ATOMICS_WAIT_ERROR: &[u8] =
  b"TypeError: cannot block in this thread";
const V8_ATOMICS_WAIT_ERROR: &[u8] =
  b"TypeError: Atomics.wait cannot be called in this context";

#[repr(C)]
struct MaybeMirror<T> {
  has_value: bool,
  value: T,
}

#[inline]
unsafe fn maybe_set<T: Copy>(out: *mut Maybe<T>, val: T) {
  if out.is_null() {
    return;
  }
  unsafe {
    ptr::write(
      out as *mut MaybeMirror<T>,
      MaybeMirror {
        has_value: true,
        value: val,
      },
    );
  }
}

#[inline]
unsafe fn maybe_none<T: Copy + Default>(out: *mut Maybe<T>) {
  if out.is_null() {
    return;
  }
  unsafe {
    ptr::write(
      out as *mut MaybeMirror<T>,
      MaybeMirror {
        has_value: false,
        value: T::default(),
      },
    );
  }
}

#[inline]
fn ctx() -> *mut JSContext {
  current_ctx()
}

#[inline]
fn is_obj(this: *const Value) -> bool {
  !this.is_null() && jsv_is_object(&jsval_of(this))
}

fn class_tag_is(this: *const Value, tag: &str) -> bool {
  let c = ctx();
  if c.is_null() || !is_obj(this) {
    return false;
  }
  let v = jsval_of(this);
  unsafe {
    let global = JS_GetGlobalObject(c);

    let obj_ctor =
      JS_GetPropertyStr(c, global, b"Object\0".as_ptr() as *const c_char);

    let proto =
      JS_GetPropertyStr(c, obj_ctor, b"prototype\0".as_ptr() as *const c_char);

    let ts =
      JS_GetPropertyStr(c, proto, b"toString\0".as_ptr() as *const c_char);

    let mut result = false;
    if jsv_is_object(&ts) {
      let r = JS_Call(c, ts, v, 0, ptr::null_mut());
      if !jsv_is_exception(&r) {
        let cstr = JS_ToCString(c, r);
        if !cstr.is_null() {
          let got = std::ffi::CStr::from_ptr(cstr)
            .to_string_lossy()
            .into_owned();
          JS_FreeCString(c, cstr);
          result = got == format!("[object {}]", tag);
        }
      } else {
        let exc = JS_GetException(c);
        JS_FreeValue(c, exc);
      }
      JS_FreeValue(c, r);
    }

    JS_FreeValue(c, ts);
    JS_FreeValue(c, proto);
    JS_FreeValue(c, obj_ctor);
    JS_FreeValue(c, global);
    result
  }
}

#[inline]
fn typed_array_type(this: *const Value) -> i32 {
  if !is_obj(this) {
    return -1;
  }
  unsafe { JS_GetTypedArrayType(jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
  !this.is_null() && jsv_is_bool(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTrue(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  jsv_is_bool(&v) && unsafe { v.u.int32 != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFalse(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  jsv_is_bool(&v) && unsafe { v.u.int32 == 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
  !this.is_null() && jsv_is_null(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
  !this.is_null() && jsv_is_undefined(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNullOrUndefined(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  jsv_is_null(&v) || jsv_is_undefined(&v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
  !this.is_null() && jsv_is_string(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbol(this: *const Value) -> bool {
  !this.is_null() && jsv_is_symbol(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsName(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  jsv_is_string(&v) || jsv_is_symbol(&v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
  !this.is_null() && jsv_is_number(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt(this: *const Value) -> bool {
  !this.is_null() && jsv_is_bigint(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
  is_obj(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);

  if jsv_is_int(&v) {
    return true;
  }

  if jsv_is_float64(&v) {
    let n = unsafe { v.u.float64 };
    return n.is_finite()
      && n.fract() == 0.0
      && n >= i32::MIN as f64
      && n <= i32::MAX as f64;
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  if jsv_is_int(&v) {
    return unsafe { v.u.int32 >= 0 };
  }
  if jsv_is_float64(&v) {
    let n = unsafe { v.u.float64 };
    return n.is_finite()
      && n.fract() == 0.0
      && n >= 0.0
      && n <= u32::MAX as f64;
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArray(this: *const Value) -> bool {
  let c = ctx();
  !c.is_null() && !this.is_null() && unsafe { JS_IsArray(jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
  let c = ctx();
  !c.is_null() && !this.is_null() && unsafe { JS_IsFunction(c, jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDate(this: *const Value) -> bool {
  class_tag_is(this, "Date")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArgumentsObject(this: *const Value) -> bool {
  class_tag_is(this, "Arguments")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigIntObject(this: *const Value) -> bool {
  is_obj(this) && class_tag_is(this, "BigInt")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBooleanObject(this: *const Value) -> bool {
  class_tag_is(this, "Boolean")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumberObject(this: *const Value) -> bool {
  class_tag_is(this, "Number")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsStringObject(this: *const Value) -> bool {
  class_tag_is(this, "String")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbolObject(this: *const Value) -> bool {
  class_tag_is(this, "Symbol")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNativeError(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  unsafe { JS_IsError(jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsRegExp(this: *const Value) -> bool {
  class_tag_is(this, "RegExp")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsAsyncFunction(this: *const Value) -> bool {
  // Native [[FunctionKind]] check (bit0 = async; async-generators set it too).
  // Robust even when the prototype/Symbol.toStringTag is removed, matching v8.
  if this.is_null() {
    return false;
  }
  unsafe { js_v82jsc_function_kind(jsval_of(this)) & 1 != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorFunction(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  unsafe { js_v82jsc_function_kind(jsval_of(this)) & 2 != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsPromise(this: *const Value) -> bool {
  !this.is_null() && unsafe { JS_IsPromise(jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMap(this: *const Value) -> bool {
  class_tag_is(this, "Map")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSet(this: *const Value) -> bool {
  class_tag_is(this, "Set")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMapIterator(this: *const Value) -> bool {
  class_tag_is(this, "Map Iterator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetIterator(this: *const Value) -> bool {
  class_tag_is(this, "Set Iterator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetGeneratorObject(this: *const Value) -> bool {
  class_tag_is(this, "Generator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakMap(this: *const Value) -> bool {
  class_tag_is(this, "WeakMap")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakSet(this: *const Value) -> bool {
  class_tag_is(this, "WeakSet")
}

unsafe extern "C" {
  fn JS_IsWeakRef(val: JSValue) -> bool;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakRef(this: *const Value) -> bool {
  !this.is_null() && unsafe { JS_IsWeakRef(jsval_of(this)) }
}

/// WebAssembly GC's `null` is a distinct value only inside a Wasm GC engine.
/// This backend's WebAssembly has no GC proposal, so no JS value is ever one.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmNull(_this: *const Value) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsProxy(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  unsafe { JS_IsProxy(jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsModuleNamespaceObject(
  this: *const Value,
) -> bool {
  class_tag_is(this, "Module")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsExternal(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  super::function::value_is_external(jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmMemoryObject(this: *const Value) -> bool {
  let _ = this;
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmModuleObject(this: *const Value) -> bool {
  super::wasm::is_module_object(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBuffer(this: *const Value) -> bool {
  class_tag_is(this, "ArrayBuffer")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSharedArrayBuffer(this: *const Value) -> bool {
  class_tag_is(this, "SharedArrayBuffer")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDataView(this: *const Value) -> bool {
  class_tag_is(this, "DataView")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTypedArray(this: *const Value) -> bool {
  typed_array_type(this) >= 0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBufferView(this: *const Value) -> bool {
  v8__Value__IsTypedArray(this) || v8__Value__IsDataView(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_UINT8
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8ClampedArray(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_UINT8C
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt8Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_INT8
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint16Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_UINT16
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt16Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_INT16
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_UINT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_INT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat32Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_FLOAT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat64Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_FLOAT64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt64Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_BIG_INT64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigUint64Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_BIG_UINT64
}

const _: i32 = JS_TYPED_ARRAY_FLOAT16;

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(
  this: *const Value,
  that: *const Value,
) -> bool {
  let c = ctx();
  if c.is_null() || this.is_null() || that.is_null() {
    return false;
  }
  let this_smi_zero = is_smi_zero(this);
  let that_smi_zero = is_smi_zero(that);
  if this_smi_zero || that_smi_zero {
    if this_smi_zero && that_smi_zero {
      return true;
    }
    let other = if this_smi_zero { that } else { this };
    let other = jsval_of(other);
    return jsv_is_number(&other) && num_of(&other) == 0.0;
  }
  let a = jsval_of(this);
  let b = jsval_of(that);
  if jsv_is_number(&a) && jsv_is_number(&b) {
    let na = num_of(&a);
    let nb = num_of(&b);
    return !na.is_nan() && !nb.is_nan() && na == nb;
  }
  unsafe { JS_IsStrictEqual(c, a, b) }
}

#[inline]
fn is_smi_zero<T>(p: *const T) -> bool {
  !p.is_null() && !is_interned_handle(p) && unsafe { *(p as *const usize) == 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBigInt(
  this: *const Value,
  context: *const Context,
) -> *const BigInt {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = jsval_of(this);
  if jsv_is_bigint(&v) {
    return intern_dup::<BigInt>(c, v);
  }
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToObject(
  this: *const Value,
  context: *const Context,
) -> *const Object {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let o = JS_ToObject(c, jsval_of(this));
    if jsv_is_exception(&o) {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      return ptr::null();
    }
    intern::<Object>(o)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInteger(
  this: *const Value,
  context: *const Context,
) -> *const Integer {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut n: f64 = 0.0;
    if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      return ptr::null();
    }

    let truncated = if n.is_nan() { 0.0 } else { n.trunc() };

    let v = JS_NewFloat64(c, truncated);
    intern::<Integer>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__BooleanValue(
  this: *const Value,
  _isolate: *mut RealIsolate,
) -> bool {
  let c = ctx();
  if c.is_null() || this.is_null() {
    return false;
  }

  unsafe { JS_ToBool(c, jsval_of(this)) > 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBoolean(
  this: *const Value,
  _isolate: *mut RealIsolate,
) -> *const Boolean {
  let c = ctx();
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let b = JS_ToBool(c, jsval_of(this));
    let v = JS_NewBool(c, if b > 0 { 1 } else { 0 });
    intern::<Boolean>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<f64>,
) {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut n: f64 = 0.0;
    if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      maybe_none(out);
    } else {
      maybe_set(out, n);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__InstanceOf(
  this: *const Value,
  context: *const Context,
  object: *const crate::Object,
  out: *mut Maybe<bool>,
) {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() || object.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let r = JS_IsInstanceOf(c, jsval_of(this), jsval_of(object));
    if r < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      maybe_none(out);
    } else {
      maybe_set(out, r != 0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Int32Value(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<i32>,
) {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut i: i32 = 0;
    if JS_ToInt32(c, &mut i, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      maybe_none(out);
    } else {
      maybe_set(out, i);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__TypeOf(
  this: *const Value,
  _isolate: *mut RealIsolate,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = jsval_of(this);

  let s: &[u8] = match v.tag {
    JS_TAG_UNDEFINED | JS_TAG_UNINITIALIZED => b"undefined\0",
    JS_TAG_NULL => b"object\0",
    JS_TAG_BOOL => b"boolean\0",
    JS_TAG_INT | JS_TAG_FLOAT64 => b"number\0",
    JS_TAG_STRING | JS_TAG_STRING_ROPE => b"string\0",
    JS_TAG_SYMBOL => b"symbol\0",
    JS_TAG_BIG_INT | JS_TAG_SHORT_BIG_INT => b"bigint\0",
    JS_TAG_OBJECT => {
      if unsafe { JS_IsFunction(ctx, v) } {
        b"function\0"
      } else {
        b"object\0"
      }
    }
    _ => b"object\0",
  };
  let js = unsafe { JS_NewString(ctx, s.as_ptr() as *const c_char) };
  intern::<V8String>(js)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToNumber(
  this: *const Value,
  context: *const Context,
) -> *const Number {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut n: f64 = 0.0;
    if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      return ptr::null();
    }
    let v = JS_NewFloat64(c, n);
    intern::<Number>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IntegerValue(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<i64>,
) {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut n: f64 = 0.0;
    if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      maybe_none(out);
      return;
    }
    let i = if n.is_nan() {
      0
    } else if n >= i64::MAX as f64 {
      i64::MAX
    } else if n <= i64::MIN as f64 {
      i64::MIN
    } else {
      n.trunc() as i64
    };
    maybe_set(out, i);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Uint32Value(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<u32>,
) {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut i: i32 = 0;
    if JS_ToInt32(c, &mut i, jsval_of(this)) < 0 {
      let exc = JS_GetException(c);
      JS_FreeValue(c, exc);
      maybe_none(out);
      return;
    }
    maybe_set(out, i as u32);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__SameValue(
  this: *const Value,
  that: *const Value,
) -> bool {
  if this.is_null() || that.is_null() {
    return false;
  }
  let a = jsval_of(this);
  let b = jsval_of(that);

  if jsv_is_number(&a) && jsv_is_number(&b) {
    let na = num_of(&a);
    let nb = num_of(&b);
    if na.is_nan() && nb.is_nan() {
      return true;
    }
    if na == 0.0 && nb == 0.0 {
      return na.is_sign_negative() == nb.is_sign_negative();
    }
    return na == nb;
  }
  let c = ctx();
  if c.is_null() {
    return false;
  }
  unsafe { JS_IsStrictEqual(c, a, b) }
}

#[inline]
fn num_of(v: &JSValue) -> f64 {
  if jsv_is_int(v) {
    unsafe { v.u.int32 as f64 }
  } else {
    unsafe { v.u.float64 }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat16Array(this: *const Value) -> bool {
  typed_array_type(this) == JS_TYPED_ARRAY_FLOAT16
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToString(
  this: *const Value,
  context: *const Context,
) -> *const V8String {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = jsval_of(this);

  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let text = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let s = if unsafe { JS_IsError(v) } && text == QUICKJS_ATOMICS_WAIT_ERROR {
    unsafe {
      JS_NewStringLen(
        ctx,
        V8_ATOMICS_WAIT_ERROR.as_ptr() as *const c_char,
        V8_ATOMICS_WAIT_ERROR.len(),
      )
    }
  } else {
    unsafe { JS_NewStringLen(ctx, cstr, len) }
  };
  unsafe { JS_FreeCString(ctx, cstr) };
  if s.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<V8String>(s)
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__GetHash(this: *const Value) -> u32 {
  if this.is_null() {
    return 0;
  }

  let v = jsval_of(this);
  if jsv_is_object(&v) {
    return super::object::v8__Object__GetIdentityHash(this as *const Object)
      as u32;
  }
  if jsv_is_string(&v) || jsv_is_symbol(&v) {
    return super::object::v8__Name__GetIdentityHash(this as *const crate::Name)
      as u32;
  }
  if jsv_is_number(&v) {
    let bits = match v.tag {
      JS_TAG_INT => unsafe { (v.u.int32 as f64).to_bits() },
      JS_TAG_FLOAT64 => unsafe {
        let n = v.u.float64;
        if n == 0.0 {
          0
        } else if n.is_nan() {
          f64::NAN.to_bits()
        } else {
          n.to_bits()
        }
      },
      _ => 0,
    };
    return hash_payload(0x6e75_6d62_6572, bits);
  }
  if jsv_is_bigint(&v) {
    let ctx = current_ctx();
    if !ctx.is_null() {
      let mut len = 0usize;
      let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
      if !cstr.is_null() {
        let bytes =
          unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
        let hash = hash_bytes(0x6269_6769_6e74, bytes);
        unsafe { JS_FreeCString(ctx, cstr) };
        return hash;
      }
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    }
  }

  let payload = match v.tag {
    JS_TAG_BOOL => unsafe { v.u.int32 as u64 },
    JS_TAG_NULL | JS_TAG_UNDEFINED | JS_TAG_UNINITIALIZED
    | JS_TAG_CATCH_OFFSET | JS_TAG_EXCEPTION => 0,
    _ => jsv_get_ptr(&v) as u64,
  };
  hash_payload(v.tag as u64, payload)
}

fn hash_payload(seed: u64, payload: u64) -> u32 {
  let mut x = payload ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
  x ^= x >> 33;
  x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
  x ^= x >> 33;
  x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
  x ^= x >> 33;
  x as u32
}

fn hash_bytes(seed: u64, bytes: &[u8]) -> u32 {
  let mut h = seed ^ (bytes.len() as u64).wrapping_mul(0x9e37_79b9);
  for &b in bytes {
    h ^= b as u64;
    h = h.wrapping_mul(0x100_0000_01b3);
  }
  hash_payload(seed, h)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToDetailString(
  this: *const Value,
  context: *const Context,
) -> *const V8String {
  v8__Value__ToString(this, context)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInt32(
  this: *const Value,
  context: *const Context,
) -> *const Int32 {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  let mut out: i32 = 0;
  if unsafe { JS_ToInt32(c, &mut out, jsval_of(this)) } < 0 {
    let exc = unsafe { JS_GetException(c) };
    unsafe { JS_FreeValue(c, exc) };
    return ptr::null();
  }
  intern::<Int32>(unsafe { JS_NewInt32(c, out) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToUint32(
  this: *const Value,
  context: *const Context,
) -> *const Uint32 {
  let c = ctx_of(context);
  if c.is_null() || this.is_null() {
    return ptr::null();
  }
  let mut out: i32 = 0;
  if unsafe { JS_ToInt32(c, &mut out, jsval_of(this)) } < 0 {
    let exc = unsafe { JS_GetException(c) };
    unsafe { JS_FreeValue(c, exc) };
    return ptr::null();
  }
  intern::<Uint32>(unsafe { JS_NewUint32(c, out as u32) })
}
