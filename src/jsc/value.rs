#![allow(non_snake_case, non_upper_case_globals, unused)]

use crate::jsc::core::{ctx_of, current_ctx, intern, intern_ctx, jsval};
use crate::jsc::jsc_sys::*;
use crate::support::Maybe;
use crate::{
  BigInt, Boolean, Context, Int32, Integer, Number, Object, RealIsolate,
  String as V8String, Uint32, Value,
};
use std::os::raw::{c_char, c_void};
use std::ptr;

#[inline]
fn ctx() -> JSContextRef {
  current_ctx()
}

#[inline]
fn jsty(v: *const Value) -> JSType {
  let c = ctx();
  if c.is_null() {
    return kJSTypeUndefined;
  }
  unsafe { JSValueGetType(c, jsval(v)) }
}

#[inline]
fn is_obj(v: *const Value) -> bool {
  let c = ctx();
  !c.is_null() && unsafe { JSValueIsObject(c, jsval(v)) }
}

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
  ptr::write(
    out as *mut MaybeMirror<T>,
    MaybeMirror {
      has_value: true,
      value: val,
    },
  );
}

#[inline]
unsafe fn maybe_none<T: Copy + Default>(out: *mut Maybe<T>) {
  if out.is_null() {
    return;
  }
  ptr::write(
    out as *mut MaybeMirror<T>,
    MaybeMirror {
      has_value: false,
      value: T::default(),
    },
  );
}

fn class_tag_is(v: *const Value, tag: &str) -> bool {
  let c = ctx();
  if c.is_null() || !is_obj(v) {
    return false;
  }
  unsafe {
    let global = JSContextGetGlobalObject(c);
    let mut exc: JSValueRef = ptr::null();

    let name =
      JSStringCreateWithUTF8CString(b"Object\0".as_ptr() as *const c_char);
    let obj_ctor = JSObjectGetProperty(c, global, name, &mut exc);
    JSStringRelease(name);
    if obj_ctor.is_null() {
      return false;
    }
    let obj_ctor_o = JSValueToObject(c, obj_ctor, &mut exc);
    if obj_ctor_o.is_null() {
      return false;
    }

    let pname =
      JSStringCreateWithUTF8CString(b"prototype\0".as_ptr() as *const c_char);
    let proto = JSObjectGetProperty(c, obj_ctor_o, pname, &mut exc);
    JSStringRelease(pname);
    let proto_o = JSValueToObject(c, proto, &mut exc);
    if proto_o.is_null() {
      return false;
    }

    let tsname =
      JSStringCreateWithUTF8CString(b"toString\0".as_ptr() as *const c_char);
    let ts = JSObjectGetProperty(c, proto_o, tsname, &mut exc);
    JSStringRelease(tsname);
    let ts_o = JSValueToObject(c, ts, &mut exc);
    if ts_o.is_null() {
      return false;
    }

    let this_o = JSValueToObject(c, jsval(v), &mut exc);
    let result =
      JSObjectCallAsFunction(c, ts_o, this_o, 0, ptr::null(), &mut exc);
    if result.is_null() {
      return false;
    }
    let s = JSValueToStringCopy(c, result, &mut exc);
    if s.is_null() {
      return false;
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max);
    JSStringRelease(s);
    let got = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char)
      .to_string_lossy()
      .into_owned();
    got == format!("[object {}]", tag)
  }
}

fn instance_of_global(v: *const Value, ctor_name: &[u8]) -> bool {
  let c = ctx();
  if c.is_null() || !is_obj(v) {
    return false;
  }
  unsafe {
    let global = JSContextGetGlobalObject(c);
    let mut exc: JSValueRef = ptr::null();
    let name =
      JSStringCreateWithUTF8CString(ctor_name.as_ptr() as *const c_char);
    let ctor = JSObjectGetProperty(c, global, name, &mut exc);
    JSStringRelease(name);
    if ctor.is_null() || !JSValueIsObject(c, ctor) {
      return false;
    }
    let ctor_o = JSValueToObject(c, ctor, &mut exc);
    if ctor_o.is_null() {
      return false;
    }
    JSValueIsInstanceOfConstructor(c, jsval(v), ctor_o, &mut exc)
  }
}

#[inline]
fn typed_array_type(v: *const Value) -> JSTypedArrayType {
  let c = ctx();
  if c.is_null() || !is_obj(v) {
    return kJSTypedArrayTypeNone;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let o = JSValueToObject(c, jsval(v), &mut exc);
    if o.is_null() {
      return kJSTypedArrayTypeNone;
    }
    JSValueGetTypedArrayType(c, o, &mut exc)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
  jsty(this) == kJSTypeUndefined
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
  jsty(this) == kJSTypeNull
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNullOrUndefined(this: *const Value) -> bool {
  let t = jsty(this);
  t == kJSTypeNull || t == kJSTypeUndefined
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTrue(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() {
    return false;
  }
  unsafe {
    JSValueIsBoolean(c, jsval(this)) && JSValueToBoolean(c, jsval(this))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__BooleanValue(
  this: *const Value,
  _isolate: *mut crate::RealIsolate,
) -> bool {
  let c = ctx();
  if c.is_null() {
    return false;
  }

  unsafe { JSValueToBoolean(c, jsval(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__TypeOf(
  this: *const Value,
  _isolate: *mut crate::RealIsolate,
) -> *const V8String {
  let c = ctx();
  if c.is_null() {
    return ptr::null();
  }

  let s: &[u8] = match jsty(this) {
    kJSTypeUndefined => b"undefined\0",
    kJSTypeNull => b"object\0",
    kJSTypeBoolean => b"boolean\0",
    kJSTypeNumber => b"number\0",
    kJSTypeString => b"string\0",
    kJSTypeSymbol => b"symbol\0",
    kJSTypeBigInt => b"bigint\0",
    kJSTypeObject => unsafe {
      let o = jsval(this) as JSObjectRef;
      if JSObjectIsFunction(c, o) {
        b"function\0"
      } else {
        b"object\0"
      }
    },
    _ => b"object\0",
  };
  unsafe {
    let js = JSStringCreateWithUTF8CString(s.as_ptr() as *const c_char);
    let v = JSValueMakeString(c, js);
    JSStringRelease(js);
    intern_ctx::<V8String>(c, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
  jsty(this) == kJSTypeString
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbol(this: *const Value) -> bool {
  jsty(this) == kJSTypeSymbol
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
  is_obj(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt(this: *const Value) -> bool {
  jsty(this) == kJSTypeBigInt
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
  jsty(this) == kJSTypeBoolean
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
  jsty(this) == kJSTypeNumber
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() || !v8__Value__IsNumber(this) {
    return false;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(c, jsval(this), &mut exc) };
  n.is_finite()
    && n.fract() == 0.0
    && n >= i32::MIN as f64
    && n <= i32::MAX as f64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() || !v8__Value__IsNumber(this) {
    return false;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(c, jsval(this), &mut exc) };
  n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArray(this: *const Value) -> bool {
  let c = ctx();
  !c.is_null() && unsafe { JSValueIsArray(c, jsval(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() || !is_obj(this) {
    return false;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let o = JSValueToObject(c, jsval(this), &mut exc);
    !o.is_null() && JSObjectIsFunction(c, o)
  }
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
  class_tag_is(this, "BigInt") && is_obj(this) && jsty(this) != kJSTypeBigInt
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
  instance_of_global(this, b"Error\0")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsRegExp(this: *const Value) -> bool {
  class_tag_is(this, "RegExp")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsAsyncFunction(this: *const Value) -> bool {
  // v8 reports async generators as both async and generator functions, so the
  // console can label them "AsyncGeneratorFunction".
  class_tag_is(this, "AsyncFunction")
    || class_tag_is(this, "AsyncGeneratorFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorFunction(this: *const Value) -> bool {
  class_tag_is(this, "GeneratorFunction")
    || class_tag_is(this, "AsyncGeneratorFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsPromise(this: *const Value) -> bool {
  // v8's IsPromise checks the internal [[PromiseState]] slot, not the prototype
  // chain — e.g. `Object.create(Promise.prototype)` is NOT a promise. Mirror
  // that with the native JSPromise downcast (returns < 0 for non-promises).
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  unsafe {
    crate::jsc::introspect::v82jsc_promise_status(
      ctx,
      jsval(this),
      ptr::null_mut(),
    ) >= 0
  }
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

/// Brand check: `WeakRef.prototype.deref` throws unless its receiver has a
/// [[WeakRefTarget]] slot, so unlike the `toStringTag` test this cannot be
/// fooled by an object that merely claims to be a WeakRef.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakRef(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() || !is_obj(this) {
    return false;
  }
  let r = unsafe {
    crate::jsc::core::call_js_fn(
      c,
      c"(function(v){try{WeakRef.prototype.deref.call(v);return true}\
         catch(e){return false}})",
      &[jsval(this)],
    )
  };
  matches!(r, Ok(v) if unsafe { JSValueToBoolean(c, v) })
}

/// WasmGC's `null` reference is a value of its own in V8. WebAssembly runs on
/// an interpreter here that does not implement WasmGC, so no such value can
/// ever reach the embedder.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmNull(_this: *const Value) -> bool {
  false
}

// JSObjectGetProxyTarget (JSObjectRefPrivate.h, a C SPI exported by vendored +
// system JSC) returns the wrapped target of a Proxy/JSProxy, or null for a
// non-proxy — exactly the detection v8::Value::IsProxy needs.
unsafe extern "C" {
  pub(crate) fn JSObjectGetProxyTarget(obj: JSObjectRef) -> JSObjectRef;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsProxy(this: *const Value) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let v = jsval(this);
  if !unsafe { JSValueIsObject(ctx, v) } {
    return false;
  }
  !unsafe { JSObjectGetProxyTarget(v as JSObjectRef) }.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsModuleNamespaceObject(
  this: *const Value,
) -> bool {
  class_tag_is(this, "Module")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsExternal(this: *const Value) -> bool {
  crate::jsc::function::value_is_external(crate::jsc::core::jsval(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmMemoryObject(this: *const Value) -> bool {
  instance_of_global(this, b"WebAssembly\0")
    && class_tag_is(this, "WebAssembly.Memory")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmModuleObject(this: *const Value) -> bool {
  class_tag_is(this, "WebAssembly.Module")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBuffer(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeArrayBuffer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBufferView(this: *const Value) -> bool {
  v8__Value__IsTypedArray(this) || v8__Value__IsDataView(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTypedArray(this: *const Value) -> bool {
  let t = typed_array_type(this);
  t != kJSTypedArrayTypeNone && t != kJSTypedArrayTypeArrayBuffer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeUint8Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8ClampedArray(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeUint8ClampedArray
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt8Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeInt8Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint16Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeUint16Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt16Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeInt16Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeUint32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeInt32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat32Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeFloat32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat64Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeFloat64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt64Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeBigInt64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigUint64Array(this: *const Value) -> bool {
  typed_array_type(this) == kJSTypedArrayTypeBigUint64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDataView(this: *const Value) -> bool {
  class_tag_is(this, "DataView")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSharedArrayBuffer(this: *const Value) -> bool {
  class_tag_is(this, "SharedArrayBuffer")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(
  this: *const Value,
  that: *const Value,
) -> bool {
  // Comparing needs no context in V8; a bare HandleScope still compares.
  let mut c = ctx();
  if c.is_null() {
    c = crate::jsc::core::ensure_ctx(crate::jsc::core::current_iso());
  }
  !c.is_null() && unsafe { JSValueIsStrictEqual(c, jsval(this), jsval(that)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBigInt(
  this: *const Value,
  context: *const Context,
) -> *const BigInt {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    return ptr::null();
  }
  if unsafe { JSValueGetType(c, jsval(this)) } == kJSTypeBigInt {
    return intern_ctx::<BigInt>(c, jsval(this));
  }
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToString(
  this: *const Value,
  context: *const Context,
) -> *const V8String {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(c, jsval(this), &mut exc);
    if s.is_null() {
      return ptr::null();
    }
    let v = JSValueMakeString(c, s);
    JSStringRelease(s);
    intern_ctx::<V8String>(c, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToObject(
  this: *const Value,
  context: *const Context,
) -> *const Object {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let o = JSValueToObject(c, jsval(this), &mut exc);
    if o.is_null() {
      return ptr::null();
    }
    intern_ctx::<Object>(c, o as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInteger(
  this: *const Value,
  context: *const Context,
) -> *const Integer {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if !exc.is_null() {
      return ptr::null();
    }
    let truncated = if n.is_nan() { 0.0 } else { n.trunc() };
    let v = JSValueMakeNumber(c, truncated);
    intern_ctx::<Integer>(c, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBoolean(
  this: *const Value,
  isolate: *mut RealIsolate,
) -> *const Boolean {
  let _ = isolate;
  let c = ctx();
  if c.is_null() {
    return ptr::null();
  }
  unsafe {
    let b = JSValueToBoolean(c, jsval(this));
    let v = JSValueMakeBoolean(c, b);
    intern_ctx::<Boolean>(c, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<f64>,
) {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if exc.is_null() {
      maybe_set(out, n);
    } else {
      maybe_none(out);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFalse(this: *const Value) -> bool {
  let c = ctx();
  if c.is_null() {
    return false;
  }
  unsafe {
    JSValueIsBoolean(c, jsval(this)) && !JSValueToBoolean(c, jsval(this))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsName(this: *const Value) -> bool {
  let t = jsty(this);
  t == kJSTypeString || t == kJSTypeSymbol
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat16Array(_this: *const Value) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__SameValue(
  this: *const Value,
  that: *const Value,
) -> bool {
  let c = ctx();
  if c.is_null() {
    return false;
  }

  unsafe {
    let a = jsval(this);
    let b = jsval(that);
    if JSValueIsStrictEqual(c, a, b) {
      if JSValueIsNumber(c, a) && JSValueIsNumber(c, b) {
        let mut exc: JSValueRef = ptr::null();
        let na = JSValueToNumber(c, a, &mut exc);
        let nb = JSValueToNumber(c, b, &mut exc);
        if na == 0.0 && nb == 0.0 {
          return na.is_sign_negative() == nb.is_sign_negative();
        }
      }
      return true;
    }

    if JSValueIsNumber(c, a) && JSValueIsNumber(c, b) {
      let mut exc: JSValueRef = ptr::null();
      let na = JSValueToNumber(c, a, &mut exc);
      let nb = JSValueToNumber(c, b, &mut exc);
      return na.is_nan() && nb.is_nan();
    }
    false
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__InstanceOf(
  this: *const Value,
  context: *const Context,
  object: *const Object,
  out: *mut Maybe<bool>,
) {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() || object.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let ctor = jsval(object as *const Value) as JSObjectRef;
    let mut exc: JSValueRef = ptr::null();
    let r = JSValueIsInstanceOfConstructor(c, jsval(this), ctor, &mut exc);
    if exc.is_null() {
      maybe_set(out, r);
    } else {
      maybe_none(out);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToNumber(
  this: *const Value,
  context: *const Context,
) -> *const Number {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if !exc.is_null() {
      return ptr::null();
    }
    let v = JSValueMakeNumber(c, n);
    intern_ctx::<Number>(c, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IntegerValue(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<i64>,
) {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if exc.is_null() {
      let i = if n.is_nan() { 0 } else { n.trunc() as i64 };
      maybe_set(out, i);
    } else {
      maybe_none(out);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Uint32Value(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<u32>,
) {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if exc.is_null() && n.is_finite() {
      let i = n.trunc().rem_euclid(4294967296.0);
      maybe_set(out, i as u32);
    } else {
      maybe_none(out);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Int32Value(
  this: *const Value,
  context: *const Context,
  out: *mut Maybe<i32>,
) {
  let c = ctx_of(context) as JSContextRef;
  if c.is_null() {
    unsafe { maybe_none(out) };
    return;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let n = JSValueToNumber(c, jsval(this), &mut exc);
    if exc.is_null() && n.is_finite() {
      let i = n.trunc().rem_euclid(4294967296.0);
      let i = if i >= 2147483648.0 {
        i - 4294967296.0
      } else {
        i
      };
      maybe_set(out, i as i32);
    } else {
      maybe_none(out);
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
pub extern "C" fn v8__Value__GetHash(
  _this: *const std::os::raw::c_void,
) -> u32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToDetailString(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInt32(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToUint32(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}
