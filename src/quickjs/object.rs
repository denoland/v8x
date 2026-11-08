//! QuickJS-backed `object` family shims: v8::Object Get/Set/Has/Delete/
//! CreateDataProperty/DefineOwnProperty/GetOwnPropertyNames/Prototype/Private,
//! plus v8::Array New/Length.
//!
//! Ported from reference/qjs_v8_compat/src/object.rs (which uses lossy
//! string keys) but upgraded to atom-based property access via
//! `JS_ValueToAtom` so symbol and integer keys round-trip correctly — this
//! matches the JSC backend's key-aware `*ForKey` calls in src/object.rs.
//!
//! Refcount discipline (see core): every fresh `JSValue` returned by a
//! QuickJS getter is +1 and goes through `intern`. Values handed to
//! `JS_SetProperty`/`JS_DefinePropertyValue` are *consumed* by QuickJS, so we
//! `JS_DupValue` first because the caller's handle slot still owns its copy.
//! Atoms from `JS_ValueToAtom`/`JS_NewAtom` are freed with `JS_FreeAtom`.
#![allow(non_snake_case, unused)]

use super::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use super::quickjs_sys::*;
use crate::support::{Maybe, MaybeBool, int};
use crate::{
  AccessorNameGetterCallback, AccessorNameSetterCallback, Array, Context,
  IndexFilter, KeyCollectionMode, KeyConversionMode, Name, Object, Private,
  PropertyAttribute, PropertyDescriptor, PropertyFilter, RealIsolate,
  String as V8String, Value,
};
use std::os::raw::{c_char, c_int};
use std::ptr;

unsafe extern "C" {
  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_GetOwnProperty(
    ctx: *mut JSContext,
    desc: *mut JSPropertyDescriptorQjs,
    obj: JSValue,
    prop: JSAtom,
  ) -> c_int;
  fn JS_GetPrototype(ctx: *mut JSContext, obj: JSValue) -> JSValue;
  fn JS_SetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
  ) -> c_int;
  fn JS_DefinePropertyValue(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto_val: JSValue,
  ) -> c_int;
  fn JS_GetLength(ctx: *mut JSContext, obj: JSValue, pres: *mut i64) -> c_int;
  fn JS_ToBool(ctx: *mut JSContext, val: JSValue) -> c_int;
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_FreePropertyEnum(
    ctx: *mut JSContext,
    tab: *mut JSPropertyEnum,
    len: u32,
  );
  fn JS_DupAtom(ctx: *mut JSContext, v: JSAtom) -> JSAtom;
}

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

#[repr(C)]
struct JSPropertyDescriptorQjs {
  flags: c_int,
  value: JSValue,
  getter: JSValue,
  setter: JSValue,
}

const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_SYMBOL_MASK: c_int = 1 << 1;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;
const JS_PROP_GETSET_QJS: c_int = 1 << 4;

unsafe fn real_named_property_descriptor(
  ctx: *mut JSContext,
  object: JSValue,
  atom: JSAtom,
) -> Result<Option<JSPropertyDescriptorQjs>, ()> {
  let mut current = unsafe { JS_DupValue(ctx, object) };
  loop {
    if !jsv_is_object(&current) {
      unsafe { JS_FreeValue(ctx, current) };
      return Ok(None);
    }
    if !super::function::is_named_handler_object(current) {
      let mut desc = JSPropertyDescriptorQjs {
        flags: 0,
        value: jsv_undefined(),
        getter: jsv_undefined(),
        setter: jsv_undefined(),
      };
      let result = unsafe { JS_GetOwnProperty(ctx, &mut desc, current, atom) };
      if result < 0 {
        unsafe { JS_FreeValue(ctx, current) };
        return Err(());
      }
      if result > 0 {
        unsafe { JS_FreeValue(ctx, current) };
        return Ok(Some(desc));
      }
    }
    let prototype = unsafe { JS_GetPrototype(ctx, current) };
    unsafe { JS_FreeValue(ctx, current) };
    if jsv_is_exception(&prototype) {
      return Err(());
    }
    if jsv_is_null(&prototype) {
      unsafe { JS_FreeValue(ctx, prototype) };
      return Ok(None);
    }
    current = prototype;
  }
}

unsafe fn free_property_descriptor(
  ctx: *mut JSContext,
  desc: JSPropertyDescriptorQjs,
) {
  unsafe {
    JS_FreeValue(ctx, desc.value);
    JS_FreeValue(ctx, desc.getter);
    JS_FreeValue(ctx, desc.setter);
  }
}

fn is_private_atom(ctx: *mut JSContext, atom: JSAtom) -> bool {
  let iso = current_iso();
  if iso.is_null() {
    return false;
  }

  iso_state(iso).private_symbols.iter().any(|&(symbol, _)| {
    let private_atom = unsafe { JS_ValueToAtom(ctx, symbol) };
    if private_atom == 0 {
      return false;
    }
    let matches = private_atom == atom;
    unsafe { JS_FreeAtom(ctx, private_atom) };
    matches
  })
}

#[inline]
fn iso_ctx(isolate: *mut RealIsolate) -> *mut JSContext {
  if isolate.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(isolate);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[inline]
fn just_bool(b: bool) -> MaybeBool {
  if b {
    MaybeBool::JustTrue
  } else {
    MaybeBool::JustFalse
  }
}

#[inline]
pub(crate) fn key_atom<T>(ctx: *mut JSContext, key: *const T) -> JSAtom {
  if key.is_null() {
    return 0;
  }
  unsafe { JS_ValueToAtom(ctx, jsval_of(key)) }
}

fn preserve_snapshot_creator_init_delete(
  ctx: *mut JSContext,
  atom: JSAtom,
) -> bool {
  let iso = current_iso();
  if iso.is_null() || !iso_state(iso).is_snapshot_creator {
    return false;
  }
  matches!(
    unsafe { atom_to_path_segment(ctx, atom) }.as_deref(),
    Some("__setTickInfo") | Some("__setImmediateInfo") | Some("__setTimerInfo")
  )
}

#[inline]
fn attr_to_jsprop(attr: u32) -> c_int {
  let mut flags = JS_PROP_HAS_VALUE
    | JS_PROP_HAS_WRITABLE
    | JS_PROP_HAS_ENUMERABLE
    | JS_PROP_HAS_CONFIGURABLE;
  if attr & 1 == 0 {
    flags |= JS_PROP_WRITABLE;
  }
  if attr & 2 == 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if attr & 4 == 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  flags
}

#[inline]
fn jsprop_to_attr(desc: &JSPropertyDescriptorQjs) -> u32 {
  let mut attr = 0;
  let read_only = if desc.flags & JS_PROP_GETSET_QJS != 0 {
    !jsv_is_object(&desc.setter)
  } else {
    desc.flags & JS_PROP_WRITABLE == 0
  };
  if read_only {
    attr |= 1;
  }
  if desc.flags & JS_PROP_ENUMERABLE == 0 {
    attr |= 2;
  }
  if desc.flags & JS_PROP_CONFIGURABLE == 0 {
    attr |= 4;
  }
  attr
}

#[inline]
unsafe fn clear_exception(ctx: *mut JSContext) {
  let exc = unsafe { JS_GetException(ctx) };
  unsafe { JS_FreeValue(ctx, exc) };
}

const JS_PROP_HAS_CONFIGURABLE: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE: c_int = 1 << 10;
const JS_PROP_HAS_VALUE: c_int = 1 << 13;

#[derive(Clone, Copy)]
struct ObjectAccessorEntry {
  getter: AccessorNameGetterCallback,
  setter: Option<AccessorNameSetterCallback>,
  data: JSValue,
  atom: JSAtom,
  attr: u32,
  lazy: bool,
}

thread_local! {
    static OBJECT_ACCESSORS: std::cell::RefCell<Vec<ObjectAccessorEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn register_object_accessor(
  ctx: *mut JSContext,
  atom: JSAtom,
  getter: AccessorNameGetterCallback,
  setter: Option<AccessorNameSetterCallback>,
  data: JSValue,
  attr: u32,
  lazy: bool,
) -> c_int {
  let data = if jsv_is_undefined(&data) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, data) }
  };
  let atom = unsafe { JS_DupAtom(ctx, atom) };
  OBJECT_ACCESSORS.with(|entries| {
    let mut entries = entries.borrow_mut();
    let magic = entries.len() as c_int;
    entries.push(ObjectAccessorEntry {
      getter,
      setter,
      data,
      atom,
      attr,
      lazy,
    });
    magic
  })
}

fn lookup_object_accessor(magic: c_int) -> Option<ObjectAccessorEntry> {
  if magic < 0 {
    return None;
  }
  OBJECT_ACCESSORS.with(|entries| entries.borrow().get(magic as usize).copied())
}

unsafe fn object_accessor_cfunc(
  ctx: *mut JSContext,
  trampoline: unsafe extern "C" fn(
    *mut JSContext,
    JSValue,
    c_int,
    *mut JSValue,
    c_int,
  ) -> JSValue,
  magic: c_int,
) -> JSValue {
  let f = unsafe {
    std::mem::transmute::<
      unsafe extern "C" fn(
        *mut JSContext,
        JSValue,
        c_int,
        *mut JSValue,
        c_int,
      ) -> JSValue,
      JSCFunction,
    >(trampoline)
  };
  unsafe {
    JS_NewCFunction2(ctx, f, ptr::null(), 0, JS_CFUNC_GENERIC_MAGIC, magic)
  }
}

unsafe extern "C" fn object_accessor_getter_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some(entry) = lookup_object_accessor(magic) else {
    return jsv_undefined();
  };
  let result = unsafe {
    super::function::call_accessor_name_getter(
      ctx,
      this_val,
      entry.atom,
      entry.data,
      entry.getter,
    )
  };
  if entry.lazy && !jsv_is_exception(&result) {
    let value = unsafe { JS_DupValue(ctx, result) };
    let _ = unsafe {
      JS_DefinePropertyValue(
        ctx,
        this_val,
        entry.atom,
        value,
        attr_to_jsprop(entry.attr),
      )
    };
  }
  result
}

unsafe extern "C" fn object_accessor_setter_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some(entry) = lookup_object_accessor(magic) else {
    return jsv_undefined();
  };
  let Some(setter) = entry.setter else {
    return jsv_undefined();
  };
  let value = if argc > 0 && !argv.is_null() {
    unsafe { *argv }
  } else {
    jsv_undefined()
  };
  unsafe {
    super::function::call_accessor_name_setter(
      ctx, this_val, entry.atom, value, entry.data, setter,
    )
  }
}

#[inline]
fn accessor_attr_to_jsprop(attr: u32) -> c_int {
  let mut flags = JS_PROP_THROW;
  if attr & 2 == 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if attr & 4 == 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  flags
}

pub(crate) fn define_native_accessor_value(
  ctx: *mut JSContext,
  obj: JSValue,
  key: JSValue,
  getter: AccessorNameGetterCallback,
  setter: Option<AccessorNameSetterCallback>,
  data: JSValue,
  attr: u32,
  lazy: bool,
) -> c_int {
  if ctx.is_null() || !jsv_is_object(&obj) {
    return -1;
  }
  let atom = unsafe { JS_ValueToAtom(ctx, key) };
  if atom == 0 {
    return -1;
  }
  let magic =
    register_object_accessor(ctx, atom, getter, setter, data, attr, lazy);
  let get = unsafe {
    object_accessor_cfunc(ctx, object_accessor_getter_trampoline, magic)
  };
  let set = if setter.is_some() && attr & 1 == 0 {
    unsafe {
      object_accessor_cfunc(ctx, object_accessor_setter_trampoline, magic)
    }
  } else {
    jsv_undefined()
  };
  let r = unsafe {
    JS_DefinePropertyGetSet(
      ctx,
      obj,
      atom,
      get,
      set,
      accessor_attr_to_jsprop(attr),
    )
  };
  unsafe { JS_FreeAtom(ctx, atom) };
  r
}

#[inline]
fn read_attr(attr: &PropertyAttribute) -> u32 {
  unsafe { *(attr as *const PropertyAttribute as *const u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let o = unsafe { JS_NewObject(ctx) };
  if o.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let h = intern::<Object>(o);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New__with_prototype_and_properties(
  isolate: *mut RealIsolate,
  prototype_or_null: *const Value,
  names: *const *const Name,
  values: *const *const Value,
  length: usize,
) -> *const Object {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let o = unsafe { JS_NewObject(ctx) };
  if o.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  unsafe {
    if !prototype_or_null.is_null() {
      let proto = jsval_of(prototype_or_null);
      if !jsv_is_undefined(&proto) {
        JS_SetPrototype(ctx, o, proto);
      }
    }
    for i in 0..length {
      let key = *names.add(i);
      let val = *values.add(i);
      let atom = key_atom(ctx, key);
      if atom == 0 {
        continue;
      }

      let v = JS_DupValue(ctx, jsval_of(val));
      JS_SetProperty(ctx, o, atom, v);
      JS_FreeAtom(ctx, atom);
    }
  }
  intern::<Object>(o)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Get(
  this: *const Object,
  context: *const Context,
  key: *const Value,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return ptr::null();
  }
  let v = unsafe { JS_GetProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }

  let h = intern::<Value>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_GetPropertyUint32(ctx, jsval_of(this), index) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrototype(
  this: *const Object,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  let v = unsafe { JS_GetPrototype(ctx, jsval_of(this)) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Set(
  this: *const Object,
  context: *const Context,
  key: *const Value,
  value: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }

  match unsafe { real_named_property_descriptor(ctx, jsval_of(this), atom) } {
    Ok(Some(desc)) if jsv_is_object(&desc.setter) => {
      let mut args = [jsval_of(value)];
      let result = unsafe {
        JS_Call(ctx, desc.setter, jsval_of(this), 1, args.as_mut_ptr())
      };
      unsafe {
        free_property_descriptor(ctx, desc);
        JS_FreeAtom(ctx, atom);
      }
      if jsv_is_exception(&result) {
        return MaybeBool::Nothing;
      }
      unsafe { JS_FreeValue(ctx, result) };
      return MaybeBool::JustTrue;
    }
    Ok(Some(desc)) => unsafe { free_property_descriptor(ctx, desc) },
    Ok(None) => {}
    Err(()) => {
      unsafe { JS_FreeAtom(ctx, atom) };
      return MaybeBool::Nothing;
    }
  }

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r = unsafe { JS_SetProperty(ctx, jsval_of(this), atom, v) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    if unsafe { swallow_write_rejection(ctx) } {
      return MaybeBool::JustFalse;
    }
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

unsafe fn swallow_write_rejection(ctx: *mut JSContext) -> bool {
  if !unsafe { JS_HasException(ctx) } {
    return true;
  }
  let exc = unsafe { JS_GetException(ctx) };
  let cs = unsafe { JS_ToCString(ctx, exc) };
  let is_write_reject = if cs.is_null() {
    false
  } else {
    let msg = unsafe { std::ffi::CStr::from_ptr(cs) }.to_string_lossy();
    let m = msg.as_ref();
    m.contains("read-only")
      || m.contains("not extensible")
      || m.contains("non-extensible")
  };
  if !cs.is_null() {
    unsafe { JS_FreeCString(ctx, cs) };
  }
  if is_write_reject {
    unsafe { JS_FreeValue(ctx, exc) };
    true
  } else {
    unsafe { JS_Throw(ctx, exc) };
    false
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
  value: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r = unsafe { JS_SetPropertyUint32(ctx, jsval_of(this), index, v) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrototype(
  this: *const Object,
  context: *const Context,
  prototype: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }

  let r = unsafe { JS_SetPrototype(ctx, jsval_of(this), jsval_of(prototype)) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CreateDataProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  value: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r = unsafe {
    JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, JS_PROP_C_W_E)
  };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineOwnProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  value: *const Value,
  attr: PropertyAttribute,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let flags = attr_to_jsprop(read_attr(&attr));
  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r =
    unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  let raw_attr = read_attr(&attr);
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  desc: *const PropertyDescriptor,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() || desc.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let r = super::property::pd_define(ctx, jsval_of(this), atom, desc);
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyDescriptor(
  this: *const Object,
  context: *const Context,
  key: *const Name,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    JS_FreeValue(ctx, global);
    let func =
      JS_GetPropertyStr(ctx, object_ctor, c"getOwnPropertyDescriptor".as_ptr());
    JS_FreeValue(ctx, object_ctor);
    let mut args = [jsval_of(this), jsval_of(key)];
    let res = JS_Call(ctx, func, jsv_undefined(), 2, args.as_mut_ptr());
    JS_FreeValue(ctx, func);
    if res.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }

    intern::<Value>(res)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Delete(
  this: *const Object,
  context: *const Context,
  key: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  if preserve_snapshot_creator_init_delete(ctx, atom) {
    unsafe { JS_FreeAtom(ctx, atom) };
    return MaybeBool::JustTrue;
  }
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Has(
  this: *const Object,
  context: *const Context,
  key: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasOwnProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let want = key_atom(ctx, key);
  if want == 0 {
    return MaybeBool::Nothing;
  }
  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      jsval_of(this),
      JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK,
    )
  };
  if rc < 0 {
    unsafe { JS_FreeAtom(ctx, want) };
    return MaybeBool::Nothing;
  }
  let mut found = false;
  unsafe {
    for i in 0..len as usize {
      if (*tab.add(i)).atom == want {
        found = true;
        break;
      }
    }
    JS_FreePropertyEnum(ctx, tab, len);
    JS_FreeAtom(ctx, want);
  }
  just_bool(found)
}

unsafe fn atom_to_path_segment(
  ctx: *mut JSContext,
  atom: JSAtom,
) -> Option<String> {
  let key = unsafe { JS_AtomToString(ctx, atom) };
  if key.tag == JS_TAG_EXCEPTION {
    return None;
  }
  let cstr = unsafe { JS_ToCString(ctx, key) };
  let out = if cstr.is_null() {
    None
  } else {
    Some(
      unsafe { std::ffi::CStr::from_ptr(cstr) }
        .to_string_lossy()
        .into_owned(),
    )
  };
  if !cstr.is_null() {
    unsafe { JS_FreeCString(ctx, cstr) };
  }
  unsafe { JS_FreeValue(ctx, key) };
  out
}

unsafe fn find_inferred_constructor_path(
  ctx: *mut JSContext,
  object: JSValue,
  target: JSValue,
  prefix: &str,
  depth: u8,
  visited: &mut std::collections::HashSet<usize>,
) -> Option<String> {
  if depth > 3 || !jsv_is_object(&object) {
    return None;
  }
  if !visited.insert(jsv_get_ptr(&object) as usize) {
    return None;
  }

  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      object,
      JS_GPN_STRING_MASK | JS_GPN_ENUM_ONLY,
    )
  };
  if rc < 0 {
    return None;
  }

  let mut found = None;
  unsafe {
    for i in 0..len as usize {
      let atom = (*tab.add(i)).atom;
      let Some(segment) = atom_to_path_segment(ctx, atom) else {
        continue;
      };
      if segment.is_empty() {
        continue;
      }
      let path = if prefix.is_empty() {
        segment
      } else {
        format!("{prefix}.{segment}")
      };
      let value = JS_GetProperty(ctx, object, atom);
      if value.tag == JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, JS_GetException(ctx));
        continue;
      }
      if JS_IsStrictEqual(ctx, value, target) {
        JS_FreeValue(ctx, value);
        found = Some(path);
        break;
      }
      if jsv_is_object(&value) && !JS_IsFunction(ctx, value) {
        found = find_inferred_constructor_path(
          ctx,
          value,
          target,
          &path,
          depth + 1,
          visited,
        );
        if found.is_some() {
          JS_FreeValue(ctx, value);
          break;
        }
      }
      JS_FreeValue(ctx, value);
    }
    JS_FreePropertyEnum(ctx, tab, len);
  }
  found
}

unsafe fn inferred_constructor_name(
  ctx: *mut JSContext,
  ctor: JSValue,
) -> JSValue {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let mut visited = std::collections::HashSet::new();
  let path = unsafe {
    find_inferred_constructor_path(ctx, global, ctor, "", 0, &mut visited)
  };
  unsafe { JS_FreeValue(ctx, global) };
  if let Some(path) = path {
    unsafe { JS_NewStringLen(ctx, path.as_ptr() as *const c_char, path.len()) }
  } else {
    jsv_null()
  }
}

unsafe fn has_own_property_atom(
  ctx: *mut JSContext,
  object: JSValue,
  want: JSAtom,
) -> bool {
  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(ctx, &mut tab, &mut len, object, JS_GPN_STRING_MASK)
  };
  if rc < 0 {
    return false;
  }

  let mut found = false;
  unsafe {
    for i in 0..len as usize {
      if (*tab.add(i)).atom == want {
        found = true;
        break;
      }
    }
    JS_FreePropertyEnum(ctx, tab, len);
  }
  found
}

unsafe fn constructor_from_prototype_chain(
  ctx: *mut JSContext,
  object: JSValue,
  ctor_atom: JSAtom,
) -> JSValue {
  let mut proto = unsafe { JS_GetPrototype(ctx, object) };
  let mut depth = 0;
  while jsv_is_object(&proto) && depth < 64 {
    if unsafe { has_own_property_atom(ctx, proto, ctor_atom) } {
      let ctor = unsafe { JS_GetProperty(ctx, proto, ctor_atom) };
      unsafe { JS_FreeValue(ctx, proto) };
      return ctor;
    }
    let next = unsafe { JS_GetPrototype(ctx, proto) };
    unsafe { JS_FreeValue(ctx, proto) };
    proto = next;
    depth += 1;
  }
  if proto.tag == JS_TAG_EXCEPTION {
    return proto;
  }
  unsafe { JS_FreeValue(ctx, proto) };
  unsafe { JS_GetProperty(ctx, object, ctor_atom) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetConstructorName(
  this: *const Object,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  unsafe {
    let fallback = || intern::<V8String>(JS_NewString(ctx, c"Object".as_ptr()));

    let drain_exc = || {
      if JS_HasException(ctx) {
        JS_FreeValue(ctx, JS_GetException(ctx));
      }
    };
    let ctor_atom = JS_NewAtom(ctx, c"constructor".as_ptr());
    let ctor = constructor_from_prototype_chain(ctx, jsval_of(this), ctor_atom);
    JS_FreeAtom(ctx, ctor_atom);
    if ctor.tag == JS_TAG_EXCEPTION {
      drain_exc();
      return fallback();
    }
    if !jsv_is_object(&ctor) {
      JS_FreeValue(ctx, ctor);
      return fallback();
    }
    let name_atom = JS_NewAtom(ctx, c"name".as_ptr());
    let name = JS_GetProperty(ctx, ctor, name_atom);
    JS_FreeAtom(ctx, name_atom);
    if name.tag == JS_TAG_EXCEPTION {
      drain_exc();
      JS_FreeValue(ctx, ctor);
      return fallback();
    }
    if !jsv_is_string(&name) {
      JS_FreeValue(ctx, name);
      JS_FreeValue(ctx, ctor);
      return fallback();
    }
    let cstr = JS_ToCString(ctx, name);
    let is_empty =
      cstr.is_null() || std::ffi::CStr::from_ptr(cstr).to_bytes().is_empty();
    if !cstr.is_null() {
      JS_FreeCString(ctx, cstr);
    }
    if is_empty {
      let inferred = inferred_constructor_name(ctx, ctor);
      if jsv_is_string(&inferred) {
        JS_FreeValue(ctx, name);
        JS_FreeValue(ctx, ctor);
        return intern::<V8String>(inferred);
      }
      JS_FreeValue(ctx, inferred);
    }

    JS_FreeValue(ctx, ctor);
    intern::<V8String>(name)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIdentityHash(this: *const Object) -> int {
  let v = jsval_of(this);
  let p = jsv_get_ptr(&v) as usize;
  let h = (p as u32) ^ ((p >> 32) as u32);
  (h & 0x7fff_ffff) as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Name__GetIdentityHash(this: *const Name) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 1;
  }
  let atom = unsafe { JS_ValueToAtom(ctx, jsval_of(this)) };
  if atom == 0 {
    return 1;
  }
  unsafe { JS_FreeAtom(ctx, atom) };
  let h = (atom ^ 0x9e37_79b9).wrapping_mul(2654435761);
  ((h & 0x7fff_ffff) | 1) as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyNames(
  this: *const Object,
  context: *const Context,
  filter: PropertyFilter,
  key_conversion: KeyConversionMode,
) -> *const Array {
  property_names_array_filtered(
    this,
    context,
    gpn_from_filter(&filter),
    false,
    false,
    key_conversion,
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyNames(
  this: *const Object,
  context: *const Context,
  mode: KeyCollectionMode,
  property_filter: PropertyFilter,
  index_filter: IndexFilter,
  key_conversion: KeyConversionMode,
) -> *const Array {
  let include_prototypes = matches!(mode, KeyCollectionMode::IncludePrototypes);
  let skip_indices = index_filter as u32 == 1;
  property_names_array_filtered(
    this,
    context,
    gpn_from_filter(&property_filter),
    skip_indices,
    include_prototypes,
    key_conversion,
  )
}

fn gpn_from_filter(filter: &PropertyFilter) -> c_int {
  let f = read_filter(filter);
  let mut gpn = 0;
  if f & 8 == 0 {
    gpn |= JS_GPN_STRING_MASK;
  }
  if f & 16 == 0 {
    gpn |= JS_GPN_SYMBOL_MASK;
  }
  if gpn == 0 {
    gpn = JS_GPN_STRING_MASK;
  }
  if f & 2 != 0 {
    gpn |= JS_GPN_ENUM_ONLY;
  }
  gpn
}

#[inline]
fn read_filter(f: &PropertyFilter) -> u32 {
  unsafe { *(f as *const PropertyFilter as *const u32) }
}

const JS_ATOM_TAG_INT: u32 = 1 << 31;

fn property_names_array_filtered(
  this: *const Object,
  context: *const Context,
  gpn_flags: c_int,
  skip_indices: bool,
  include_prototypes: bool,
  key_conversion: KeyConversionMode,
) -> *const Array {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let mut seen = std::collections::HashSet::new();
  let mut out = 0u32;

  unsafe {
    let mut object = JS_DupValue(ctx, jsval_of(this));
    loop {
      if object.tag != JS_TAG_OBJECT {
        JS_FreeValue(ctx, object);
        break;
      }
      if !append_own_property_names(
        ctx,
        object,
        gpn_flags,
        skip_indices,
        key_conversion,
        arr,
        &mut out,
        &mut seen,
      ) {
        JS_FreeValue(ctx, object);
        JS_FreeValue(ctx, arr);
        return ptr::null();
      }
      if !include_prototypes {
        JS_FreeValue(ctx, object);
        break;
      }
      let prototype = JS_GetPrototype(ctx, object);
      JS_FreeValue(ctx, object);
      if prototype.tag == JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, arr);
        return ptr::null();
      }
      object = prototype;
    }
  }
  intern::<Array>(arr)
}

unsafe fn append_own_property_names(
  ctx: *mut JSContext,
  object: JSValue,
  gpn_flags: c_int,
  skip_indices: bool,
  key_conversion: KeyConversionMode,
  arr: JSValue,
  out: &mut u32,
  seen: &mut std::collections::HashSet<JSAtom>,
) -> bool {
  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(ctx, &mut tab, &mut len, object, gpn_flags)
  };
  if rc < 0 {
    return false;
  }

  let mut ok = true;
  unsafe {
    for i in 0..len as usize {
      let atom = (*tab.add(i)).atom;
      if is_private_atom(ctx, atom) {
        continue;
      }
      let is_index = (atom & JS_ATOM_TAG_INT) != 0;
      if skip_indices || key_conversion as u32 == 2 {
        if is_index {
          continue;
        }
      }
      if !seen.insert(atom) {
        continue;
      }

      let key_val = if is_index && key_conversion as u32 == 0 {
        JS_AtomToString(ctx, atom)
      } else if is_index {
        let index = atom & !JS_ATOM_TAG_INT;
        if index <= i32::MAX as u32 {
          jsv_int32(index as i32)
        } else {
          jsv_float64(index as f64)
        }
      } else {
        JS_AtomToValue(ctx, atom)
      };
      if key_val.tag == JS_TAG_EXCEPTION {
        ok = false;
        break;
      }
      if JS_SetPropertyUint32(ctx, arr, *out, key_val) < 0 {
        ok = false;
        break;
      }
      *out += 1;
    }
    JS_FreePropertyEnum(ctx, tab, len);
  }
  ok
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return ptr::null();
  }
  let v = unsafe { JS_GetProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
  value: *const Value,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let flags = JS_PROP_HAS_VALUE
    | JS_PROP_HAS_WRITABLE
    | JS_PROP_HAS_CONFIGURABLE
    | JS_PROP_WRITABLE
    | JS_PROP_CONFIGURABLE;
  let r =
    unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[inline]
fn index_atom(ctx: *mut JSContext, index: u32) -> JSAtom {
  let v = unsafe { JS_NewInt64(ctx, index as i64) };
  let a = unsafe { JS_ValueToAtom(ctx, v) };
  unsafe { JS_FreeValue(ctx, v) };
  a
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = index_atom(ctx, index);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DeleteIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = index_atom(ctx, index);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasPrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DeletePrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return ptr::null();
  }
  let desc =
    unsafe { real_named_property_descriptor(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  let Ok(Some(desc)) = desc else {
    return ptr::null();
  };
  let value = if jsv_is_object(&desc.getter) {
    unsafe { JS_Call(ctx, desc.getter, jsval_of(this), 0, ptr::null_mut()) }
  } else {
    unsafe { JS_DupValue(ctx, desc.value) }
  };
  unsafe { free_property_descriptor(ctx, desc) };
  if jsv_is_exception(&value) {
    return ptr::null();
  }
  intern::<Value>(value)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasRealNamedProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return MaybeBool::Nothing;
  }
  let desc =
    unsafe { real_named_property_descriptor(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  match desc {
    Ok(Some(desc)) => {
      unsafe { free_property_descriptor(ctx, desc) };
      just_bool(true)
    }
    Ok(None) => just_bool(false),
    Err(()) => MaybeBool::Nothing,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedPropertyAttributes(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  out: *mut Maybe<PropertyAttribute>,
) {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    unsafe { maybe_attr_none(out) };
    return;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    unsafe { maybe_attr_none(out) };
    return;
  }
  let desc =
    unsafe { real_named_property_descriptor(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  match desc {
    Ok(Some(desc)) => unsafe {
      maybe_attr_set(out, jsprop_to_attr(&desc));
      free_property_descriptor(ctx, desc);
    },
    Ok(None) | Err(()) => unsafe { maybe_attr_none(out) },
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetCreationContext(
  this: *const Object,
) -> *const Context {
  let _ = this;
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  super::core::intern_ctx(ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIntegrityLevel(
  this: *const Object,
  context: *const Context,
  level: crate::IntegrityLevel,
) -> MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }

  let method: &[u8] = match level {
    crate::IntegrityLevel::Frozen => b"freeze\0",
    crate::IntegrityLevel::Sealed => b"seal\0",
  };
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    JS_FreeValue(ctx, global);
    let func =
      JS_GetPropertyStr(ctx, object_ctor, method.as_ptr() as *const c_char);
    JS_FreeValue(ctx, object_ctor);
    if !jsv_is_object(&func) {
      JS_FreeValue(ctx, func);
      return MaybeBool::Nothing;
    }
    let mut args = [JS_DupValue(ctx, jsval_of(this))];
    let r = JS_Call(ctx, func, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, func);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return MaybeBool::Nothing;
    }
    JS_FreeValue(ctx, r);
    MaybeBool::JustTrue
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__PreviewEntries(
  this: *const Object,
  is_key_value: *mut bool,
) -> *const Array {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    if !is_key_value.is_null() {
      unsafe { *is_key_value = false };
    }
    return ptr::null();
  }
  // A Map/Set ITERATOR has no `.entries()`; previewing it must peek the
  // remaining backing entries WITHOUT consuming it (Array.from would). Native
  // peek (quickjs.c js_v82jsc_iterator_preview); collections fall through.
  {
    let mut kv: std::os::raw::c_int = 0;
    let arr =
      unsafe { js_v82jsc_iterator_preview(ctx, jsval_of(this), &mut kv) };
    if arr.tag != JS_TAG_NULL && arr.tag != JS_TAG_EXCEPTION {
      if !is_key_value.is_null() {
        unsafe { *is_key_value = kv != 0 };
      }
      return intern::<Array>(arr);
    }
    if arr.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    }
  }
  unsafe {
    let is_map = {
      let m = JS_GetPropertyStr(ctx, jsval_of(this), c"set".as_ptr());
      let is_fn = JS_IsFunction(ctx, m);
      JS_FreeValue(ctx, m);
      is_fn
    };
    if !is_key_value.is_null() {
      *is_key_value = is_map;
    }

    let global = JS_GetGlobalObject(ctx);
    let array_ctor = JS_GetPropertyStr(ctx, global, c"Array".as_ptr());
    JS_FreeValue(ctx, global);
    let from = JS_GetPropertyStr(ctx, array_ctor, c"from".as_ptr());
    if !jsv_is_object(&from) {
      JS_FreeValue(ctx, from);
      JS_FreeValue(ctx, array_ctor);
      return ptr::null();
    }
    let mut args = [JS_DupValue(ctx, jsval_of(this))];
    let r = JS_Call(ctx, from, array_ctor, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, from);
    JS_FreeValue(ctx, array_ctor);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<Array>(r)
  }
}

#[inline]
unsafe fn maybe_attr_set(out: *mut Maybe<PropertyAttribute>, bits: u32) {
  if out.is_null() {
    return;
  }

  unsafe {
    let p = out as *mut u8;
    *(p as *mut bool) = true;
    let voff = std::mem::offset_of!(MaybeAttrMirror, value);
    *(p.add(voff) as *mut u32) = bits;
  }
}

#[inline]
unsafe fn maybe_attr_none(out: *mut Maybe<PropertyAttribute>) {
  if out.is_null() {
    return;
  }
  unsafe {
    *(out as *mut bool) = false;
  }
}

#[repr(C)]
struct MaybeAttrMirror {
  has_value: bool,
  value: PropertyAttribute,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyAttributes(
  this: *const Object,
  context: *const Context,
  key: *const Value,
  out: *mut Maybe<PropertyAttribute>,
) {
  let ctx = ctx_of(context);
  if out.is_null() || ctx.is_null() || this.is_null() {
    return;
  }

  let atom = key_atom(ctx, key);
  if atom == 0 {
    unsafe { maybe_attr_none(out) };
    return;
  }
  let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    unsafe { maybe_attr_none(out) };
  } else {
    unsafe { maybe_attr_set(out, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(
  isolate: *mut RealIsolate,
  length: int,
) -> *const Array {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  if length > 0 {
    unsafe { JS_SetLength_local(ctx, arr, length as i64) };
  }
  intern::<Array>(arr)
}

#[inline]
unsafe fn JS_SetLength_local(ctx: *mut JSContext, arr: JSValue, len: i64) {
  let lv = jsv_float64(len as f64);
  let atom = JS_NewAtom(ctx, c"length".as_ptr());
  unsafe { JS_SetProperty(ctx, arr, atom, lv) };
  unsafe { JS_FreeAtom(ctx, atom) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New_with_elements(
  isolate: *mut RealIsolate,
  elements: *const *const Value,
  length: usize,
) -> *const Array {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  unsafe {
    for i in 0..length {
      let el = *elements.add(i);

      let v = JS_DupValue(ctx, jsval_of(el));
      JS_SetPropertyUint32(ctx, arr, i as u32, v);
    }
  }
  intern::<Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__Length(array: *const Array) -> u32 {
  let ctx = current_ctx();
  if ctx.is_null() || array.is_null() {
    return 0;
  }
  let mut len: i64 = 0;
  let rc = unsafe { JS_GetLength(ctx, jsval_of(array), &mut len) };
  if rc < 0 || len < 0 {
    return 0;
  }
  len as u32
}

/// `v8::Array::CallbackResult`.
const ARRAY_ITERATE_EXCEPTION: i32 = 0;
const ARRAY_ITERATE_BREAK: i32 = 1;

type ArrayIterateCallback = unsafe extern "C" fn(
  index: u32,
  element: *const Value,
  data: *mut std::ffi::c_void,
) -> i32;

/// Visits each element in index order. Holes read as `undefined`, as in V8's
/// fast path. The element handle lives in a stack slot that is released after
/// the callback returns: the contract forbids the callback from letting it
/// escape, so it never needs a place in the handle scope.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__Iterate(
  this: *const Array,
  context: *const Context,
  callback: ArrayIterateCallback,
  callback_data: *mut std::ffi::c_void,
) -> bool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let array = jsval_of(this);
  let mut len: i64 = 0;
  if unsafe { JS_GetLength(ctx, array, &mut len) } < 0 {
    return false;
  }
  for index in 0..len.max(0) as u32 {
    let element = unsafe { JS_GetPropertyUint32(ctx, array, index) };
    if element.tag == JS_TAG_EXCEPTION {
      return false;
    }
    let slot = element;
    let result = unsafe {
      callback(
        index,
        &slot as *const JSValue as *const Value,
        callback_data,
      )
    };
    unsafe { JS_FreeValue(ctx, element) };
    match result {
      ARRAY_ITERATE_EXCEPTION => return false,
      ARRAY_ITERATE_BREAK => return true,
      _ => {}
    }
  }
  true
}

thread_local! {
    static WRAP_TABLE: std::cell::RefCell<
        std::collections::HashMap<(usize, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    static API_WRAPPERS: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());

    static INTERNAL_FIELDS: std::cell::RefCell<
        std::collections::HashMap<usize, Vec<JSValue>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    static ALIGNED_FIELDS: std::cell::RefCell<
        std::collections::HashMap<(usize, crate::support::int, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[inline]
fn wrap_key(wrapper: *const Object) -> usize {
  unsafe { jsval_of(wrapper).u.ptr as usize }
}

#[inline]
fn value_key(v: JSValue) -> usize {
  unsafe { v.u.ptr as usize }
}

pub(crate) fn set_internal_field_count_for_value(
  obj: JSValue,
  count: crate::support::int,
) {
  if count <= 0 || !jsv_is_object(&obj) {
    return;
  }
  let mut fields = Vec::with_capacity(count as usize);
  for _ in 0..count {
    fields.push(jsv_undefined());
  }
  INTERNAL_FIELDS.with(|t| {
    t.borrow_mut().insert(value_key(obj), fields);
  });
}

pub(crate) fn mark_api_wrapper_for_value(obj: JSValue) {
  if !jsv_is_object(&obj) {
    return;
  }
  let key = value_key(obj);
  API_WRAPPERS.with(|t| {
    t.borrow_mut().insert(key);
  });
}

pub(crate) fn internal_field_count_for_value(
  obj: JSValue,
) -> crate::support::int {
  if !jsv_is_object(&obj) {
    return 0;
  }
  let key = value_key(obj);
  INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .map(|fields| fields.len() as crate::support::int)
      .unwrap_or(0)
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Wrap(
  _isolate: *const RealIsolate,
  wrapper: *const Object,
  value: *const crate::binding::RustObj,
  tag: u16,
) {
  if wrapper.is_null() {
    return;
  }
  let key = wrap_key(wrapper);
  WRAP_TABLE.with(|t| {
    t.borrow_mut().insert((key, tag), value as usize);
  });
  super::misc::cppgc_register_wrapper(
    jsval_of(wrapper),
    value as *const crate::binding::RustObj,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Unwrap(
  _isolate: *const RealIsolate,
  wrapper: *const Object,
  tag: u16,
) -> *mut crate::binding::RustObj {
  if wrapper.is_null() {
    return ptr::null_mut();
  }
  let key = wrap_key(wrapper);
  let wrapper_value = jsval_of(wrapper);
  WRAP_TABLE.with(|t| {
    let entry = t.borrow().get(&(key, tag)).copied();
    let Some(entry) = entry else {
      return ptr::null_mut();
    };
    let value = entry as *mut crate::binding::RustObj;
    if super::misc::cppgc_wrapper_matches(value, wrapper_value) {
      value
    } else {
      t.borrow_mut().remove(&(key, tag));
      ptr::null_mut()
    }
  })
}

pub(crate) fn is_api_wrapper_value(obj: JSValue) -> bool {
  jsv_is_object(&obj)
    && API_WRAPPERS.with(|t| t.borrow().contains(&value_key(obj)))
}

/// Returns the first object on `this`'s prototype chain, starting with `this`
/// itself, that is an instance of `templ`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__FindInstanceInPrototypeChain(
  this: *const Object,
  templ: *const crate::FunctionTemplate,
) -> *const Object {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || templ.is_null() {
    return ptr::null();
  }
  let mut current = unsafe { JS_DupValue(ctx, jsval_of(this)) };
  // V8 bounds this walk too; a chain this deep is not a real class hierarchy.
  for _ in 0..64 {
    if !jsv_is_object(&current) {
      break;
    }
    if super::function::template_has_instance(ctx, current, templ) {
      return intern::<Object>(current);
    }
    let next = unsafe { JS_GetPrototype(ctx, current) };
    unsafe { JS_FreeValue(ctx, current) };
    if next.tag == JS_TAG_EXCEPTION {
      unsafe { clear_exception(ctx) };
      return ptr::null();
    }
    current = next;
  }
  unsafe { JS_FreeValue(ctx, current) };
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsApiWrapper(this: *const Object) -> bool {
  if this.is_null() {
    return false;
  }
  let key = wrap_key(this);
  API_WRAPPERS.with(|t| t.borrow().contains(&key))
    || WRAP_TABLE.with(|t| t.borrow().keys().any(|(w, _)| *w == key))
}

// Callable objects (rusty_v8 v152.0.0, denoland/rusty_v8#1893). QuickJS makes
// no distinction between a function object and a callable object, so these
// delegate to the `Function` implementations.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsCallable(this: *const Object) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  unsafe { JS_IsFunction(ctx, jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsConstructor(this: *const Object) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  unsafe { JS_IsConstructor(ctx, jsval_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CallAsFunction(
  this: *const Object,
  context: *const Context,
  recv: *const Value,
  argc: int,
  argv: *const *const Value,
) -> *const Value {
  super::function::v8__Function__Call(
    this as *const crate::Function,
    context,
    recv,
    argc,
    argv,
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CallAsConstructor(
  this: *const Object,
  context: *const Context,
  argc: int,
  argv: *const *const Value,
) -> *const Value {
  super::function::v8__Function__NewInstance(
    this as *const crate::Function,
    context,
    argc,
    argv,
  ) as *const Value
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetAlignedPointerFromInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  tag: u16,
) -> *const std::os::raw::c_void {
  if this.is_null() || index < 0 {
    return std::ptr::null();
  }
  let key = value_key(jsval_of(this));
  ALIGNED_FIELDS.with(|t| {
    t.borrow().get(&(key, index, tag)).copied().unwrap_or(0)
      as *const std::os::raw::c_void
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
) -> *const std::os::raw::c_void {
  if this.is_null() || index < 0 {
    return std::ptr::null();
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return std::ptr::null();
  }
  let key = value_key(jsval_of(this));
  INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .and_then(|fields| fields.get(index as usize).copied())
      .map(|v| intern_dup::<crate::Data>(ctx, v) as *const std::os::raw::c_void)
      .unwrap_or(std::ptr::null())
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__InternalFieldCount(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  internal_field_count_for_value(jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAccessor(
  this: *const std::os::raw::c_void,
  context: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  getter: AccessorNameGetterCallback,
  setter: Option<AccessorNameSetterCallback>,
  data_or_null: *const std::os::raw::c_void,
  attr: crate::PropertyAttribute,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context as *const Context);
  if ctx.is_null() || this.is_null() || key.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return crate::support::MaybeBool::Nothing;
  }

  let attr_bits = read_attr(&attr);
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    jsval_of(data_or_null)
  };
  let r = define_native_accessor_value(
    ctx,
    jsval_of(this),
    jsval_of(key),
    getter,
    setter,
    data,
    attr_bits,
    false,
  );
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    crate::support::MaybeBool::Nothing
  } else {
    crate::support::MaybeBool::JustTrue
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAlignedPointerInInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  value: *const std::os::raw::c_void,
  tag: u16,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let key = value_key(jsval_of(this));
  let in_range = INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .map(|fields| (index as usize) < fields.len())
      .unwrap_or(false)
  });
  if !in_range {
    return;
  }
  ALIGNED_FIELDS.with(|t| {
    t.borrow_mut().insert((key, index, tag), value as usize);
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  data: *const std::os::raw::c_void,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let key = value_key(jsval_of(this));
  let value = unsafe { JS_DupValue(ctx, jsval_of(data)) };
  INTERNAL_FIELDS.with(|t| {
    let mut t = t.borrow_mut();
    let Some(fields) = t.get_mut(&key) else {
      unsafe { JS_FreeValue(ctx, value) };
      return;
    };
    let Some(slot) = fields.get_mut(index as usize) else {
      unsafe { JS_FreeValue(ctx, value) };
      return;
    };
    let old = std::mem::replace(slot, value);
    unsafe { JS_FreeValue(ctx, old) };
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetLazyDataProperty(
  this: *const std::os::raw::c_void,
  context: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  getter: AccessorNameGetterCallback,
  data_or_null: *const std::os::raw::c_void,
  attr: crate::PropertyAttribute,
  _getter_side_effect_type: crate::SideEffectType,
  _setter_side_effect_type: crate::SideEffectType,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context as *const Context);
  if ctx.is_null() || this.is_null() || key.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  let atom = key_atom(ctx, key);
  if atom == 0 {
    return crate::support::MaybeBool::Nothing;
  }

  let attr_bits = read_attr(&attr);
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    jsval_of(data_or_null)
  };
  let r = define_native_accessor_value(
    ctx,
    jsval_of(this),
    jsval_of(key),
    getter,
    None,
    data,
    attr_bits,
    true,
  );
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    crate::support::MaybeBool::Nothing
  } else {
    crate::support::MaybeBool::JustTrue
  }
}

fn regexp_flag_string(flags: crate::RegExpCreationFlags) -> String {
  let mut out = String::new();
  if flags.contains(crate::RegExpCreationFlags::GLOBAL) {
    out.push('g');
  }
  if flags.contains(crate::RegExpCreationFlags::IGNORE_CASE) {
    out.push('i');
  }
  if flags.contains(crate::RegExpCreationFlags::MULTILINE) {
    out.push('m');
  }
  if flags.contains(crate::RegExpCreationFlags::STICKY) {
    out.push('y');
  }
  if flags.contains(crate::RegExpCreationFlags::UNICODE) {
    out.push('u');
  }
  if flags.contains(crate::RegExpCreationFlags::DOT_ALL) {
    out.push('s');
  }
  if flags.contains(crate::RegExpCreationFlags::HAS_INDICES) {
    out.push('d');
  }
  if flags.contains(crate::RegExpCreationFlags::UNICODE_SETS) {
    out.push('v');
  }
  out
}

#[inline]
fn ctx_from_raw_context(
  context: *const std::os::raw::c_void,
) -> *mut JSContext {
  let ctx = ctx_of(context as *const Context);
  if ctx.is_null() { current_ctx() } else { ctx }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__Exec(
  this: *const std::os::raw::c_void,
  context: *const std::os::raw::c_void,
  subject: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let ctx = ctx_from_raw_context(context);
  if ctx.is_null() || this.is_null() || subject.is_null() {
    return ptr::null();
  }
  unsafe {
    let this_val = jsval_of(this as *const Value);
    let exec = JS_GetPropertyStr(ctx, this_val, c"exec".as_ptr());
    if exec.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return ptr::null();
    }
    if !JS_IsFunction(ctx, exec) {
      JS_FreeValue(ctx, exec);
      return ptr::null();
    }

    let mut args = [JS_DupValue(ctx, jsval_of(subject as *const Value))];
    let result = JS_Call(ctx, exec, this_val, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, exec);
    JS_FreeValue(ctx, args[0]);
    if result.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return ptr::null();
    }
    if !jsv_is_object(&result) {
      JS_FreeValue(ctx, result);
      return ptr::null();
    }
    intern::<Object>(result) as *const std::os::raw::c_void
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__GetSource(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let source = JS_GetPropertyStr(
      ctx,
      jsval_of(this as *const Value),
      c"source".as_ptr(),
    );
    if source.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return ptr::null();
    }
    intern::<V8String>(source) as *const std::os::raw::c_void
  }
}

// --- RegExp backtrack limit
//
// libregexp has no step budget, but it polls the runtime's interrupt handler
// every `REGEXP_STEPS_PER_POLL` backtracking steps (`lre_check_timeout`). A
// regexp created with a limit gets an own `exec` that runs the original with a
// budget armed; the interrupt handler spends `REGEXP_STEPS_PER_POLL` of it per
// poll and aborts the match once it is gone. The abort surfaces as a failed
// match (null), which is what V8 reports when the limit is hit. The limit is
// therefore honoured to a granularity of one poll interval. Installing it as
// `exec` means every path that runs the regexp sees it: `RegExp::Exec`, and
// JS's `exec`, `test`, `match`, `replace` and friends, which all go through
// `RegExpExec` and so through this property.

/// libregexp's `INTERRUPT_COUNTER_INIT`.
const REGEXP_STEPS_PER_POLL: u64 = 10_000;

thread_local! {
  /// Remaining steps of the innermost limited regexp, and whether it ran out.
  static REGEXP_BUDGET: std::cell::Cell<Option<(u64, bool)>> =
    const { std::cell::Cell::new(None) };
}

/// Called from the runtime's interrupt handler. Returns true when the
/// currently armed backtrack budget is exhausted, which aborts the match.
pub(crate) fn regexp_budget_exhausted() -> bool {
  REGEXP_BUDGET.with(|b| match b.get() {
    None => false,
    Some((remaining, _)) if remaining > REGEXP_STEPS_PER_POLL => {
      b.set(Some((remaining - REGEXP_STEPS_PER_POLL, false)));
      false
    }
    Some(_) => {
      b.set(Some((0, true)));
      true
    }
  })
}

unsafe extern "C" fn limited_regexp_exec(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let original = unsafe { *data };
  let mut limit: i64 = 0;
  unsafe { JS_ToInt64(ctx, &mut limit, *data.add(1)) };
  let outer =
    REGEXP_BUDGET.with(|b| b.replace(Some((limit.max(1) as u64, false))));
  let result = unsafe { JS_Call(ctx, original, this_val, argc, argv) };
  let hit = REGEXP_BUDGET.with(|b| {
    let hit = matches!(b.get(), Some((_, true)));
    b.set(outer);
    hit
  });
  if hit && result.tag == JS_TAG_EXCEPTION {
    unsafe { clear_exception(ctx) };
    return jsv_null();
  }
  result
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__MAX_BACKTRACK_LIMIT() -> u32 {
  // V8's `kSmiMaxValue` without pointer compression.
  i32::MAX as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__NewWithBacktrackLimit(
  context: *const std::os::raw::c_void,
  pattern: *const std::os::raw::c_void,
  flags: crate::RegExpCreationFlags,
  backtrack_limit: u32,
) -> *const std::os::raw::c_void {
  let regexp = v8__RegExp__New(context, pattern, flags);
  let ctx = ctx_from_raw_context(context);
  if regexp.is_null() || ctx.is_null() {
    return regexp;
  }
  unsafe {
    let value = jsval_of(regexp as *const Value);
    let original = JS_GetPropertyStr(ctx, value, c"exec".as_ptr());
    if !JS_IsFunction(ctx, original) {
      JS_FreeValue(ctx, original);
      clear_exception(ctx);
      return regexp;
    }
    let mut data = [original, JS_NewInt64(ctx, backtrack_limit as i64)];
    let exec =
      JS_NewCFunctionData(ctx, limited_regexp_exec, 1, 0, 2, data.as_mut_ptr());
    JS_FreeValue(ctx, data[0]);
    if exec.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return regexp;
    }
    let atom = JS_NewAtom(ctx, c"exec".as_ptr());
    JS_DefinePropertyValue(
      ctx,
      value,
      atom,
      exec,
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
    JS_FreeAtom(ctx, atom);
  }
  regexp
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__New(
  context: *const std::os::raw::c_void,
  pattern: *const std::os::raw::c_void,
  flags: crate::RegExpCreationFlags,
) -> *const std::os::raw::c_void {
  let ctx = ctx_from_raw_context(context);
  if ctx.is_null() || pattern.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"RegExp".as_ptr());
    JS_FreeValue(ctx, global);
    if ctor.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return ptr::null();
    }
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }

    let flag_string = regexp_flag_string(flags);
    let mut args = [
      JS_DupValue(ctx, jsval_of(pattern as *const Value)),
      JS_NewStringLen(
        ctx,
        flag_string.as_ptr() as *const c_char,
        flag_string.len(),
      ),
    ];
    if args[1].tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      JS_FreeValue(ctx, args[0]);
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }

    let regexp = JS_CallConstructor(ctx, ctor, 2, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    if regexp.tag == JS_TAG_EXCEPTION {
      clear_exception(ctx);
      return ptr::null();
    }
    if !JS_IsRegExp(regexp) {
      JS_FreeValue(ctx, regexp);
      return ptr::null();
    }
    intern::<crate::RegExp>(regexp) as *const std::os::raw::c_void
  }
}
