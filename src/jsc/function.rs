//! JSC-backed shims for the "function" family:
//! Function / FunctionCallbackInfo / ReturnValue / Template / ObjectTemplate /
//! Signature / External.
#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::{
  Context, Data, External, Function, FunctionCallback, FunctionCallbackInfo,
  FunctionTemplate, Name, Object, ObjectTemplate, PropertyAttribute,
  RealIsolate, Signature, String, Value,
};
use std::convert::TryFrom;
use std::os::raw::{c_char, c_void};
use std::ptr;

#[repr(C)]
struct FnJSClassDefinition {
  version: std::os::raw::c_int,
  attributes: u32,
  className: *const c_char,
  parentClass: JSClassRef,
  staticValues: *const c_void,
  staticFunctions: *const c_void,
  initialize: *const c_void,
  finalize: *const c_void,
  hasProperty: *const c_void,
  getProperty: *const c_void,
  setProperty: *const c_void,
  deleteProperty: *const c_void,
  getPropertyNames: *const c_void,
  callAsFunction: *const c_void,
  callAsConstructor: *const c_void,
  hasInstance: *const c_void,
  convertToType: *const c_void,
}

#[repr(C)]
struct RawReturnValue(usize);

#[repr(C)]
struct RawFunctionCallbackInfoParts {
  isolate: *mut RealIsolate,
  return_value: usize,
  data: *const Value,
  length: crate::support::int,
}

struct FnBridge {
  callback: FunctionCallback,
  data: JSValueRef,
  ctx: JSGlobalContextRef,
}

#[repr(C)]
struct CbInfo {
  isolate: *mut RealIsolate,
  ctx: JSContextRef,
  this: JSValueRef,
  data: JSValueRef,
  new_target: JSValueRef,
  is_construct: bool,
  args: Vec<JSValueRef>,

  return_slot: Box<JSValueRef>,
}

struct FnTemplate {
  callback: FunctionCallback,
  data: JSValueRef,
  length: i32,
  class_name: Option<std::string::String>,
  proto: *mut ObjTemplate,
  instance: *mut ObjTemplate,
  parent: *const FnTemplate,

  props: Vec<(JSValueRef, JSValueRef, u32)>,

  constructable: bool,

  cached_proto: JSValueRef,

  /// `Template::SetNativeDataProperty` / `SetLazyDataProperty` entries,
  /// installed on the function object.
  natives: Vec<TemplNative>,
}

struct TemplAccessor {
  key: JSValueRef,
  getter: *const FnTemplate,
  setter: *const FnTemplate,
  attr: u32,
}

struct ObjTemplate {
  internal_field_count: i32,
  props: Vec<(JSValueRef, JSValueRef, u32)>,
  accessors: Vec<TemplAccessor>,

  /// Set by `SetCallAsFunctionHandler`: instances of this template are
  /// callable and invoke this callback with this data.
  call_handler: Option<(FunctionCallback, JSValueRef)>,

  /// `Template::SetNativeDataProperty` / `SetLazyDataProperty` entries,
  /// installed on each instance.
  natives: Vec<TemplNative>,

  parent_fn: *const FnTemplate,
}

thread_local! {
    static FN_CLASS: std::cell::Cell<JSClassRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

fn fn_class() -> JSClassRef {
  FN_CLASS.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let def = FnJSClassDefinition {
      version: 0,
      attributes: 0,
      className: c"v8jsc_fn".as_ptr(),
      parentClass: ptr::null_mut(),
      staticValues: ptr::null(),
      staticFunctions: ptr::null(),
      initialize: ptr::null(),
      finalize: fn_finalize as *const c_void,
      hasProperty: ptr::null(),
      getProperty: ptr::null(),
      setProperty: ptr::null(),
      deleteProperty: ptr::null(),
      getPropertyNames: ptr::null(),
      callAsFunction: fn_trampoline as *const c_void,
      callAsConstructor: fn_construct_trampoline as *const c_void,
      hasInstance: ptr::null(),
      convertToType: ptr::null(),
    };
    let cls =
      unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
    c.set(cls);
    cls
  })
}

unsafe extern "C" fn fn_finalize(object: JSObjectRef) {
  crate::jsc::core::ffi_guard(
    || {
      let p = unsafe { JSObjectGetPrivate(object) } as *mut FnBridge;
      if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
      }
    },
    || (),
  )
}

unsafe fn dispatch(
  ctx: JSContextRef,
  bridge: &FnBridge,
  this: JSValueRef,
  new_target: JSValueRef,
  is_construct: bool,
  argc: usize,
  argv: *const JSValueRef,
  out_exc: *mut JSValueRef,
) -> JSValueRef {
  let mut args = Vec::with_capacity(argc);
  for i in 0..argc {
    args.push(unsafe { *argv.add(i) });
  }

  let iso = current_iso();
  crate::jsc::core::clear_pending_exception(iso);
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this,
    data: bridge.data,
    new_target,
    is_construct,
    args,
    return_slot: Box::new(ptr::null()),
  });
  let info_ptr = Box::into_raw(info) as *const FunctionCallbackInfo;
  unsafe { (bridge.callback)(info_ptr) };

  let info = unsafe { Box::from_raw(info_ptr as *mut CbInfo) };
  let ret = *info.return_slot;

  let pending = crate::jsc::core::peek_pending_exception(iso);
  if !pending.is_null() {
    if !out_exc.is_null() {
      unsafe { *out_exc = pending };
    }
    crate::jsc::core::clear_pending_exception(iso);
    return unsafe { JSValueMakeUndefined(ctx) };
  }
  if ret.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    ret
  }
}

unsafe extern "C" fn fn_trampoline(
  ctx: JSContextRef,
  function: JSObjectRef,
  this_object: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  exception: *mut JSValueRef,
) -> JSValueRef {
  crate::jsc::core::ffi_guard(
    || {
      let bridge = unsafe { JSObjectGetPrivate(function) } as *const FnBridge;
      if bridge.is_null() {
        return unsafe { JSValueMakeUndefined(ctx) };
      }
      unsafe {
        dispatch(
          ctx,
          &*bridge,
          this_object as JSValueRef,
          JSValueMakeUndefined(ctx),
          false,
          argc,
          argv,
          exception,
        )
      }
    },
    || unsafe { panic_to_exception(ctx, exception) },
  )
}

/// Fallback for a panicking call/construct trampoline: surface the panic as a
/// thrown JS exception (so the JS call fails deterministically instead of
/// aborting the process) and return `undefined`.
unsafe fn panic_to_exception(
  ctx: JSContextRef,
  exception: *mut JSValueRef,
) -> JSValueRef {
  unsafe {
    if !exception.is_null() && (*exception).is_null() {
      let msg = JSStringCreateWithUTF8CString(
        c"v82jsc: rust panic in native callback".as_ptr(),
      );
      let s = JSValueMakeString(ctx, msg);
      JSStringRelease(msg);
      *exception = s;
    }
    JSValueMakeUndefined(ctx)
  }
}

unsafe extern "C" fn fn_construct_trampoline(
  ctx: JSContextRef,
  function: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  exception: *mut JSValueRef,
) -> JSObjectRef {
  crate::jsc::core::ffi_guard(
    || {
      let bridge = unsafe { JSObjectGetPrivate(function) } as *const FnBridge;
      if bridge.is_null() {
        return unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
      }

      let this = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
      unsafe {
        let proto_key = JSStringCreateWithUTF8CString(c"prototype".as_ptr());
        let mut exc: JSValueRef = ptr::null();
        let proto = JSObjectGetProperty(ctx, function, proto_key, &mut exc);
        JSStringRelease(proto_key);
        if !proto.is_null() && JSValueIsObject(ctx, proto) {
          JSObjectSetPrototype(ctx, this, proto);
        }
      }
      let r = unsafe {
        dispatch(
          ctx,
          &*bridge,
          this as JSValueRef,
          function as JSValueRef,
          true,
          argc,
          argv,
          exception,
        )
      };

      if !exception.is_null() && !unsafe { *exception }.is_null() {
        return unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
      }

      if !r.is_null() && unsafe { JSValueIsObject(ctx, r) } {
        r as JSObjectRef
      } else {
        this
      }
    },
    || unsafe {
      panic_to_exception(ctx, exception);
      JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut())
    },
  )
}

unsafe fn make_function(
  ctx: JSContextRef,
  callback: FunctionCallback,
  data: JSValueRef,
) -> JSObjectRef {
  unsafe { make_function_len(ctx, callback, data, 0) }
}

thread_local! {
    static STR_LENGTH: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_NAME: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_PROTOTYPE: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_FUNCTION: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_OBJECT: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_DEFINE_PROPERTY: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_VALUE: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
    static STR_CONFIGURABLE: std::cell::Cell<JSStringRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

#[inline]
unsafe fn cached_str(
  cell: &'static std::thread::LocalKey<std::cell::Cell<JSStringRef>>,
  lit: *const std::os::raw::c_char,
) -> JSStringRef {
  cell.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let s = unsafe { JSStringCreateWithUTF8CString(lit) };

    let s = unsafe { JSStringRetain(s) };
    c.set(s);
    s
  })
}

unsafe fn make_function_len(
  ctx: JSContextRef,
  callback: FunctionCallback,
  data: JSValueRef,
  length: i32,
) -> JSObjectRef {
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };
  if !data.is_null() {
    unsafe { JSValueProtect(gctx, data) };
  }
  let bridge = Box::new(FnBridge {
    callback,
    data,
    ctx: gctx,
  });
  let obj = unsafe {
    JSObjectMake(ctx, fn_class(), Box::into_raw(bridge) as *mut c_void)
  };

  let key = unsafe { cached_str(&STR_LENGTH, c"length".as_ptr()) };
  let lenval = unsafe { JSValueMakeNumber(ctx, length.max(0) as f64) };
  let mut exc: JSValueRef = ptr::null();
  unsafe {
    JSObjectSetProperty(ctx, obj, key, lenval, 2 | 4, &mut exc);
  }

  unsafe {
    let fp = function_prototype(ctx);
    if !fp.is_null() {
      JSObjectSetPrototype(ctx, obj, fp);
    }
  }
  obj
}

unsafe fn function_prototype(ctx: JSContextRef) -> JSValueRef {
  // Fetch `Function.prototype` FRESH from the current context every time. It
  // must NOT be cached keyed by the global-context pointer: deno_core creates
  // and disposes a JsRuntime (and thus a JSGlobalContextRef) per test, and the
  // allocator readily reuses a freed gctx's address for the next runtime. A
  // pointer-keyed cache then returns the *dangling* Function.prototype of the
  // freed context; using it as a new object's prototype corrupts JSC's
  // structure chain and sends `JSObject::didBecomePrototype` into unbounded
  // recursion → stack-overflow SIGSEGV that non-deterministically truncated the
  // whole binary (denoland/divybot#653). `Function.prototype` is a permanent
  // global of the live context, so it needs no protect.
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let fkey = cached_str(&STR_FUNCTION, c"Function".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let func_ctor = JSObjectGetProperty(ctx, global, fkey, &mut exc);
    if func_ctor.is_null() || !JSValueIsObject(ctx, func_ctor) {
      return ptr::null();
    }
    let pkey = cached_str(&STR_PROTOTYPE, c"prototype".as_ptr());
    JSObjectGetProperty(ctx, func_ctor as JSObjectRef, pkey, &mut exc)
  }
}

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
  unsafe { &mut *(this as *mut CbInfo) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
  isolate: *mut RealIsolate,
  value: *mut c_void,
) -> *const External {
  // V8 permits `External::new` on a bare HandleScope (no entered Context);
  // fall back to the isolate's base context so the object can be created.
  let ctx = crate::jsc::core::ensure_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let obj = unsafe { JSObjectMake(ctx, ext_class(), value) };
  intern_ctx::<External>(ctx, obj as JSValueRef)
}

thread_local! {
    static EXT_CLASS: std::cell::Cell<JSClassRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

fn ext_class() -> JSClassRef {
  EXT_CLASS.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let def = FnJSClassDefinition {
      version: 0,
      attributes: 0,
      className: c"v8jsc_external".as_ptr(),
      parentClass: ptr::null_mut(),
      staticValues: ptr::null(),
      staticFunctions: ptr::null(),
      initialize: ptr::null(),
      finalize: ptr::null(),
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { JSObjectGetPrivate(jsval(this) as JSObjectRef) }
}

pub(crate) fn value_is_external(v: JSValueRef) -> bool {
  if v.is_null() {
    return false;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return false;
  }
  unsafe { JSValueIsObjectOfClass(ctx, v, ext_class()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__New(
  context: *const Context,
  callback: FunctionCallback,
  data_or_null: *const Value,
  length: i32,
  constructor_behavior: crate::ConstructorBehavior,
  side_effect_type: crate::SideEffectType,
) -> *const Function {
  let _ = (constructor_behavior, side_effect_type);
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let f =
    unsafe { make_function_len(ctx, callback, jsval(data_or_null), length) };
  intern_ctx::<Function>(ctx, f as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
  this: *const Function,
  context: *const Context,
  recv: *const Value,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Value {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  // Once execution has been terminated, the JS loop only resumes through call
  // boundaries like this one (e.g. deno resolving a completed op). A tight CPU
  // loop unwinds via the watchdog, but a loop that yields on `await` does not —
  // so refuse to re-enter JS and surface an empty (undefined) pending exception
  // that deno turns into "execution terminated".
  let iso = crate::jsc::core::current_iso();
  if !iso.is_null() && crate::jsc::terminate::is_terminating(iso) {
    let undef = unsafe { JSValueMakeUndefined(ctx) };
    crate::jsc::core::record_pending_exception(ctx, undef);
    return ptr::null();
  }
  let func = jsval(this) as JSObjectRef;
  let recv_obj = if recv.is_null() {
    ptr::null_mut()
  } else {
    jsval(recv) as JSObjectRef
  };
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValueRef> = Vec::with_capacity(n);
  for i in 0..n {
    let p = unsafe { *argv.add(i) };
    args.push(jsval(p));
  }
  let mut exc: JSValueRef = ptr::null();
  let r = unsafe {
    JSObjectCallAsFunction(ctx, func, recv_obj, n, args.as_ptr(), &mut exc)
  };
  if r.is_null() {
    if !exc.is_null() && std::env::var("V82JSC_DEBUG").is_ok() {
      unsafe {
        let s = JSValueToStringCopy(ctx, exc, ptr::null_mut());
        if !s.is_null() {
          let max = JSStringGetMaximumUTF8CStringSize(s);
          let mut buf = vec![0u8; max];
          let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
          JSStringRelease(s);
          buf.truncate(n.saturating_sub(1));
          eprintln!(
            "[v82jsc] Function__Call threw: {}",
            std::string::String::from_utf8_lossy(&buf)
          );
        }
      }
    }

    if !exc.is_null() {
      crate::jsc::core::record_pending_exception(ctx, exc);
    }
    return ptr::null();
  }
  intern_ctx::<Value>(ctx, r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance(
  this: *const Function,
  context: *const Context,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Object {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let func = jsval(this) as JSObjectRef;
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValueRef> = Vec::with_capacity(n);
  for i in 0..n {
    args.push(jsval(unsafe { *argv.add(i) }));
  }
  let mut exc: JSValueRef = ptr::null();
  let r =
    unsafe { JSObjectCallAsConstructor(ctx, func, n, args.as_ptr(), &mut exc) };
  if r.is_null() {
    return ptr::null();
  }
  intern_ctx::<Object>(ctx, r as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__SetName(
  this: *const Function,
  name: *const String,
) {
  if this.is_null() || name.is_null() {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let obj = jsval(this) as JSObjectRef;
  // A function inherits `name` (value "") from `Function.prototype` as a
  // non-writable data property. A plain `[[Set]]` (`JSObjectSetProperty`) walks
  // the prototype chain, finds that non-writable `name`, and silently does
  // nothing — so the op's real name never sticks and `fn.name` reads "". Force
  // an *own*, configurable `name` via `Object.defineProperty`, matching V8's
  // function-name descriptor (writable:false, enumerable:false, configurable).
  unsafe { define_own_name(ctx, obj, jsval(name)) };
}

/// `Object.defineProperty(obj, "name", { value, writable:false,
/// enumerable:false, configurable:true })`. Used to overwrite the inherited
/// non-writable `Function.prototype.name`, which a plain `[[Set]]` can't shadow.
unsafe fn define_own_name(
  ctx: JSContextRef,
  obj: JSObjectRef,
  value: JSValueRef,
) {
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let okey = cached_str(&STR_OBJECT, c"Object".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let object_ctor = JSObjectGetProperty(ctx, global, okey, &mut exc);
    if object_ctor.is_null() || !JSValueIsObject(ctx, object_ctor) {
      return;
    }
    let dpkey = cached_str(&STR_DEFINE_PROPERTY, c"defineProperty".as_ptr());
    let define =
      JSObjectGetProperty(ctx, object_ctor as JSObjectRef, dpkey, &mut exc);
    if define.is_null() || !JSValueIsObject(ctx, define) {
      return;
    }
    let define = define as JSObjectRef;
    if !JSObjectIsFunction(ctx, define) {
      return;
    }

    let desc = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
    let value_str = cached_str(&STR_VALUE, c"value".as_ptr());
    JSObjectSetProperty(ctx, desc, value_str, value, 0, &mut exc);
    let conf_str = cached_str(&STR_CONFIGURABLE, c"configurable".as_ptr());
    JSObjectSetProperty(
      ctx,
      desc,
      conf_str,
      JSValueMakeBoolean(ctx, true),
      0,
      &mut exc,
    );

    let name_key = cached_str(&STR_NAME, c"name".as_ptr());
    let name_val = JSValueMakeString(ctx, name_key);
    let args = [obj as JSValueRef, name_val, desc as JSValueRef];
    JSObjectCallAsFunction(
      ctx,
      define,
      ptr::null_mut(),
      args.len(),
      args.as_ptr(),
      &mut exc,
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache(
  script: *const Function,
) -> *mut crate::CachedData<'static> {
  // JSC has no bytecode-cache export; return a placeholder (non-null) cache so
  // deno's CJS path (`function.create_code_cache().ok_or_else(...)`) doesn't
  // hard-error. The consume path rejects it and recompiles. Mirrors the
  // UnboundModuleScript placeholder.
  let _ = script;
  crate::jsc::module::make_placeholder_code_cache()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetIsolate(
  this: *const FunctionCallbackInfo,
) -> *mut RealIsolate {
  if this.is_null() {
    return current_iso();
  }
  cbinfo(this).isolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetParts(
  this: *const FunctionCallbackInfo,
) -> RawFunctionCallbackInfoParts {
  if this.is_null() {
    return RawFunctionCallbackInfoParts {
      isolate: current_iso(),
      return_value: 0,
      data: ptr::null(),
      length: 0,
    };
  }
  let info = cbinfo(this);
  let slot = &mut *info.return_slot as *mut JSValueRef;
  RawFunctionCallbackInfoParts {
    isolate: info.isolate,
    return_value: slot as usize,
    data: info.data as *const Value,
    length: info.args.len() as crate::support::int,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Data(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  if info.data.is_null() {
    return (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value;
  }
  info.data as *const Value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
  this: *const FunctionCallbackInfo,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  info.this as *const Object
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Get(
  this: *const FunctionCallbackInfo,
  index: crate::support::int,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  if index < 0 {
    return (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value;
  }
  match info.args.get(index as usize) {
    Some(&v) => v as *const Value,
    None => (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Length(
  this: *const FunctionCallbackInfo,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  cbinfo(this).args.len() as crate::support::int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetReturnValue(
  this: *const FunctionCallbackInfo,
) -> usize {
  if this.is_null() {
    return 0;
  }
  let info = cbinfo(this);
  (&mut *info.return_slot as *mut JSValueRef) as usize
}

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut JSValueRef {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*this).0 as *mut JSValueRef }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
  this: *mut RawReturnValue,
  value: *const Value,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsval(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
  this: *mut RawReturnValue,
  value: bool,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeBoolean(current_ctx(), value) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
  this: *mut RawReturnValue,
  value: i32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeNumber(current_ctx(), value as f64) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
  this: *mut RawReturnValue,
  value: u32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeNumber(current_ctx(), value as f64) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Double(
  this: *mut RawReturnValue,
  value: f64,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeNumber(current_ctx(), value) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeNull(current_ctx()) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetUndefined(
  this: *mut RawReturnValue,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = unsafe { JSValueMakeUndefined(current_ctx()) };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__Set(
  this: *const crate::Template,
  key: *const Name,
  value: *const Data,
  attr: PropertyAttribute,
) {
  if this.is_null() {
    return;
  }

  let raw = this as *const c_void as usize;
  with_template_props(raw, |props| {
    props.push((jsval(key), jsval(value), attr.as_u32_lenient()));
  });
}

// ---------------------------------------------------------------------------
// Native data properties (`Template::SetNativeDataProperty`,
// `SetLazyDataProperty`, and the `Object` variants).
//
// V8 stores these as AccessorInfo and calls the embedder's getter on every
// read. JSC's C API has no accessor that calls back into C with the property
// name, so each one is installed as a JS accessor pair whose functions carry a
// `NativeBridge`. The getter builds a `PropCbInfo`, which is what the
// `v8__PropertyCallbackInfo__*` accessors below read.
//
// Without a setter, a write turns the property into an ordinary data property
// holding the written value, which is what V8 does by installing
// `Accessors::ReconfigureToDataProperty` as the default setter. A lazy data
// property does the same with the getter's result on first read.
// ---------------------------------------------------------------------------

const ATTR_READ_ONLY: u32 = 1;
const ATTR_DONT_ENUM: u32 = 2;
const ATTR_DONT_DELETE: u32 = 4;

#[derive(Clone, Copy)]
struct TemplNative {
  key: JSValueRef,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data: JSValueRef,
  attr: u32,
  lazy: bool,
}

/// What a `PropertyCallbackInfo` pointer handed to a native accessor points
/// at.
#[repr(C)]
struct PropCbInfo {
  isolate: *mut RealIsolate,
  ctx: JSContextRef,
  holder: JSValueRef,
  data: JSValueRef,
  return_slot: JSValueRef,
}

struct NativeBridge {
  native: TemplNative,
  is_setter: bool,
}

thread_local! {
    static NATIVE_CLASS: std::cell::Cell<JSClassRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

fn native_class() -> JSClassRef {
  NATIVE_CLASS.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let def = FnJSClassDefinition {
      version: 0,
      attributes: 0,
      className: c"v8jsc_native_accessor".as_ptr(),
      parentClass: ptr::null_mut(),
      staticValues: ptr::null(),
      staticFunctions: ptr::null(),
      initialize: ptr::null(),
      finalize: native_finalize as *const c_void,
      hasProperty: ptr::null(),
      getProperty: ptr::null(),
      setProperty: ptr::null(),
      deleteProperty: ptr::null(),
      getPropertyNames: ptr::null(),
      callAsFunction: native_trampoline as *const c_void,
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

unsafe extern "C" fn native_finalize(object: JSObjectRef) {
  crate::jsc::core::ffi_guard(
    || {
      let p = unsafe { JSObjectGetPrivate(object) } as *mut NativeBridge;
      if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
      }
    },
    || (),
  )
}

/// `Object.defineProperty(obj, key, {value, writable, enumerable,
/// configurable})` with the flags taken from a V8 `PropertyAttribute`.
unsafe fn define_data_property(
  ctx: JSContextRef,
  obj: JSValueRef,
  key: JSValueRef,
  value: JSValueRef,
  attr: u32,
) -> Result<JSValueRef, JSValueRef> {
  unsafe {
    let args = [
      obj,
      key,
      value,
      JSValueMakeBoolean(ctx, attr & ATTR_READ_ONLY == 0),
      JSValueMakeBoolean(ctx, attr & ATTR_DONT_ENUM == 0),
      JSValueMakeBoolean(ctx, attr & ATTR_DONT_DELETE == 0),
    ];
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(o,k,v,w,e,c){Object.defineProperty(o,k,\
         {value:v,writable:w,enumerable:e,configurable:c})})",
      &args,
    )
  }
}

/// Hands a pending exception left by an embedder callback to JSC.
unsafe fn take_callback_exception(
  iso: *mut RealIsolate,
  out_exc: *mut JSValueRef,
) -> bool {
  let pending = crate::jsc::core::peek_pending_exception(iso);
  if pending.is_null() {
    return false;
  }
  if !out_exc.is_null() {
    unsafe { *out_exc = pending };
  }
  crate::jsc::core::clear_pending_exception(iso);
  true
}

unsafe extern "C" fn native_trampoline(
  ctx: JSContextRef,
  function: JSObjectRef,
  this_object: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  exception: *mut JSValueRef,
) -> JSValueRef {
  crate::jsc::core::ffi_guard(
    || unsafe {
      let undefined = JSValueMakeUndefined(ctx);
      let bridge = JSObjectGetPrivate(function) as *const NativeBridge;
      if bridge.is_null() {
        return undefined;
      }
      let bridge = &*bridge;
      let n = bridge.native;
      let holder = if this_object.is_null() {
        undefined
      } else {
        this_object as JSValueRef
      };
      let iso = current_iso();
      let data = if n.data.is_null() { undefined } else { n.data };
      let key = intern_ctx::<crate::Name>(ctx, n.key);
      let Some(key) = std::ptr::NonNull::new(key as *mut crate::Name) else {
        return undefined;
      };
      if bridge.is_setter {
        let value = if argc > 0 { *argv } else { undefined };
        match n.setter {
          Some(setter) => {
            crate::jsc::core::clear_pending_exception(iso);
            let mut info = PropCbInfo {
              isolate: iso,
              ctx,
              holder,
              data,
              return_slot: ptr::null(),
            };
            let value = intern_ctx::<Value>(ctx, value);
            if let Some(value) = std::ptr::NonNull::new(value as *mut Value) {
              setter(
                crate::SealedLocal(key),
                crate::SealedLocal(value),
                &mut info as *mut PropCbInfo
                  as *const crate::PropertyCallbackInfo<()>,
              );
            }
            take_callback_exception(iso, exception);
          }
          None if n.attr & ATTR_READ_ONLY == 0 && !this_object.is_null() => {
            if let Err(exc) =
              define_data_property(ctx, holder, n.key, value, n.attr)
              && !exception.is_null()
            {
              *exception = exc;
            }
          }
          None => {}
        }
        return undefined;
      }

      crate::jsc::core::clear_pending_exception(iso);
      let mut info = PropCbInfo {
        isolate: iso,
        ctx,
        holder,
        data,
        return_slot: ptr::null(),
      };
      (n.getter)(
        crate::SealedLocal(key),
        &mut info as *mut PropCbInfo
          as *const crate::PropertyCallbackInfo<Value>,
      );
      if take_callback_exception(iso, exception) {
        return undefined;
      }
      let ret = if info.return_slot.is_null() {
        undefined
      } else {
        info.return_slot
      };
      if n.lazy
        && !this_object.is_null()
        && let Err(exc) = define_data_property(ctx, holder, n.key, ret, n.attr)
      {
        if !exception.is_null() {
          *exception = exc;
        }
        return undefined;
      }
      ret
    },
    || unsafe { panic_to_exception(ctx, exception) },
  )
}

/// Installs `n` on `obj` as an accessor pair. Returns false if JS refused the
/// definition (for example on a non-extensible object).
fn install_native(
  ctx: JSContextRef,
  obj: JSObjectRef,
  n: &TemplNative,
) -> bool {
  if ctx.is_null() || obj.is_null() || n.key.is_null() {
    return false;
  }
  unsafe {
    let gctx = JSContextGetGlobalContext(ctx);
    JSValueProtect(gctx, n.key);
    if !n.data.is_null() {
      JSValueProtect(gctx, n.data);
    }
    let make = |is_setter: bool| {
      let bridge = Box::new(NativeBridge {
        native: *n,
        is_setter,
      });
      let f =
        JSObjectMake(ctx, native_class(), Box::into_raw(bridge) as *mut c_void);
      let fp = function_prototype(ctx);
      if !fp.is_null() {
        JSObjectSetPrototype(ctx, f, fp);
      }
      f as JSValueRef
    };
    let getter = make(false);
    let setter = make(true);
    // Lazy properties and setter-less ones redefine themselves as data
    // properties, which needs the accessor to stay configurable.
    let configurable =
      n.lazy || n.setter.is_none() || n.attr & ATTR_DONT_DELETE == 0;
    let args = [
      obj as JSValueRef,
      n.key,
      getter,
      setter,
      JSValueMakeBoolean(ctx, n.attr & ATTR_DONT_ENUM == 0),
      JSValueMakeBoolean(ctx, configurable),
    ];
    crate::jsc::core::call_js_fn(
      ctx,
      c"(function(o,k,g,s,e,c){Object.defineProperty(o,k,\
         {get:g,set:s,enumerable:e,configurable:c})})",
      &args,
    )
    .is_ok()
  }
}

fn apply_natives(ctx: JSContextRef, obj: JSObjectRef, natives: &[TemplNative]) {
  for n in natives {
    install_native(ctx, obj, n);
  }
}

/// The `data` a native accessor reports: `undefined` when none was given, as
/// `PropertyCallbackArguments::data` requires a value.
fn native_data(data_or_null: *const Value) -> JSValueRef {
  if !data_or_null.is_null() {
    return jsval(data_or_null);
  }
  let ctx = crate::jsc::core::ensure_ctx(current_iso());
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe { JSValueMakeUndefined(ctx) }
}

fn push_template_native(this: *const c_void, native: TemplNative) {
  if this.is_null() || native.key.is_null() {
    return;
  }
  let p = this as usize;
  match TEMPLATES.with(|t| t.borrow().get(&p).copied()) {
    Some(TemplKind::Func) => {
      unsafe { &mut *(p as *mut FnTemplate) }.natives.push(native)
    }
    Some(TemplKind::Obj) => unsafe { &mut *(p as *mut ObjTemplate) }
      .natives
      .push(native),
    Some(TemplKind::Dict) | None => {}
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetNativeDataProperty(
  this: *const crate::Template,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) {
  push_template_native(
    this as *const c_void,
    TemplNative {
      key: jsval(key),
      getter,
      setter,
      data: native_data(data_or_null),
      attr: attr.as_u32_lenient(),
      lazy: false,
    },
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetLazyDataProperty(
  this: *const crate::Template,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) {
  push_template_native(
    this as *const c_void,
    TemplNative {
      key: jsval(key),
      getter,
      setter: None,
      data: native_data(data_or_null),
      attr: attr.as_u32_lenient(),
      lazy: true,
    },
  );
}

/// Installs a native data property directly on an object, for
/// `Object::SetNativeDataProperty` (rusty_v8's `set_accessor`).
pub(crate) fn object_set_native(
  context: *const Context,
  obj: *const Object,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) -> bool {
  object_install_native(
    context,
    obj,
    TemplNative {
      key: jsval(key),
      getter,
      setter,
      data: native_data(data_or_null),
      attr: attr.as_u32_lenient(),
      lazy: false,
    },
  )
}

/// Installs a lazy data property directly on an object, for
/// `Object::SetLazyDataProperty`.
pub(crate) fn object_set_lazy(
  context: *const Context,
  obj: *const Object,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) -> bool {
  object_install_native(
    context,
    obj,
    TemplNative {
      key: jsval(key),
      getter,
      setter: None,
      data: native_data(data_or_null),
      attr: attr.as_u32_lenient(),
      lazy: true,
    },
  )
}

fn object_install_native(
  context: *const Context,
  obj: *const Object,
  native: TemplNative,
) -> bool {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || obj.is_null() {
    return false;
  }
  let o = jsval(obj);
  if !unsafe { JSValueIsObject(ctx, o) } {
    return false;
  }
  install_native(ctx, o as JSObjectRef, &native)
}

// ---------------------------------------------------------------------------
// DictionaryTemplate: a fixed list of property names, instantiated with one
// value per name (a missing value leaves the property out).
// ---------------------------------------------------------------------------

struct DictTemplate {
  names: Vec<std::ffi::CString>,
}

pub(crate) fn is_dictionary_template(p: *const c_void) -> bool {
  !p.is_null()
    && matches!(
      TEMPLATES.with(|t| t.borrow().get(&(p as usize)).copied()),
      Some(TemplKind::Dict)
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DictionaryTemplate__New(
  _isolate: *mut RealIsolate,
  names: *const *const c_char,
  name_lengths: *const usize,
  names_len: usize,
) -> *const c_void {
  let mut out = Vec::with_capacity(names_len);
  for i in 0..names_len {
    let bytes = unsafe {
      std::slice::from_raw_parts(
        *names.add(i) as *const u8,
        *name_lengths.add(i),
      )
    };
    // An interior NUL cannot be a JS property name passed through the C API;
    // V8 only accepts ASCII names here anyway.
    out.push(std::ffi::CString::new(bytes).unwrap_or_default());
  }
  let t = Box::into_raw(Box::new(DictTemplate { names: out }));
  register_template(t as usize, TemplKind::Dict);
  t as *const c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DictionaryTemplate__NewInstance(
  this: *const c_void,
  context: *const Context,
  values: *const *const Value,
  values_len: usize,
) -> *const Object {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || !is_dictionary_template(this) {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const DictTemplate) };
  let obj = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  for (i, name) in t.names.iter().enumerate().take(values_len) {
    let v = unsafe { *values.add(i) };
    if v.is_null() {
      continue;
    }
    unsafe {
      let key = JSStringCreateWithUTF8CString(name.as_ptr());
      let mut exc: JSValueRef = ptr::null();
      JSObjectSetProperty(ctx, obj, key, jsval(v), 0, &mut exc);
      JSStringRelease(key);
    }
  }
  intern_ctx::<Object>(ctx, obj as JSValueRef)
}

// ---------------------------------------------------------------------------
// FunctionTemplate::HasInstance
// ---------------------------------------------------------------------------

/// Whether `v` was created from `tmpl` or a template that inherits from it.
///
/// V8 answers from the object's map, which records the constructor's
/// template. JSC objects carry no such link, so this goes by the prototype
/// both construction paths give an instance: `new F()` and
/// `InstanceTemplate()->NewInstance()` set it to the template's cached
/// prototype object. An object whose prototype JS code set to that object by
/// hand is therefore also reported as an instance.
pub(crate) fn template_has_instance(
  ctx: JSContextRef,
  tmpl: *const FunctionTemplate,
  v: JSValueRef,
) -> bool {
  if ctx.is_null() || tmpl.is_null() || v.is_null() {
    return false;
  }
  if !unsafe { JSValueIsObject(ctx, v) } {
    return false;
  }
  let proto = unsafe { JSObjectGetPrototype(ctx, v as JSObjectRef) };
  if proto.is_null() {
    return false;
  }
  let target = tmpl as *const FnTemplate;
  TEMPLATES.with(|t| {
    t.borrow().iter().any(|(&p, kind)| {
      if !matches!(kind, TemplKind::Func) {
        return false;
      }
      let f = unsafe { &*(p as *const FnTemplate) };
      if f.cached_proto.is_null() || f.cached_proto != proto {
        return false;
      }
      // `f` built this object's prototype; it counts if `f` is `tmpl` or
      // inherits from it.
      let mut cur = p as *const FnTemplate;
      while !cur.is_null() {
        if cur == target {
          return true;
        }
        cur = unsafe { (*cur).parent };
      }
      false
    })
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__HasInstance(
  this: *const FunctionTemplate,
  object: *const Value,
) -> bool {
  let ctx = current_ctx();
  template_has_instance(ctx, this, jsval(object))
}

trait AttrU32 {
  fn as_u32_lenient(&self) -> u32;
}
impl AttrU32 for PropertyAttribute {
  fn as_u32_lenient(&self) -> u32 {
    unsafe { *(self as *const PropertyAttribute as *const u32) }
  }
}

thread_local! {
    static TEMPLATES: std::cell::RefCell<std::collections::HashMap<usize, TemplKind>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone, Copy)]
enum TemplKind {
  Func,
  Obj,
  Dict,
}

fn register_template(p: usize, kind: TemplKind) {
  TEMPLATES.with(|t| {
    t.borrow_mut().insert(p, kind);
  });
}

pub(crate) fn is_template_ptr(p: *const c_void) -> bool {
  if p.is_null() {
    return false;
  }
  TEMPLATES.with(|t| t.borrow().contains_key(&(p as usize)))
}

fn with_template_props(
  p: usize,
  f: impl FnOnce(&mut Vec<(JSValueRef, JSValueRef, u32)>),
) {
  let kind = TEMPLATES.with(|t| t.borrow().get(&p).copied());
  match kind {
    Some(TemplKind::Func) => {
      let t = unsafe { &mut *(p as *mut FnTemplate) };
      f(&mut t.props);
    }
    Some(TemplKind::Obj) => {
      let t = unsafe { &mut *(p as *mut ObjTemplate) };
      f(&mut t.props);
    }
    Some(TemplKind::Dict) | None => {}
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__New(
  isolate: *mut RealIsolate,
  callback: FunctionCallback,
  data_or_null: *const Value,
  signature_or_null: *const Signature,
  length: i32,
  constructor_behavior: crate::ConstructorBehavior,
  side_effect_type: crate::SideEffectType,
  c_functions: *const crate::fast_api::CFunction,
  c_functions_len: usize,
) -> *const FunctionTemplate {
  let _ = (
    isolate,
    signature_or_null,
    side_effect_type,
    c_functions,
    c_functions_len,
  );
  let constructable =
    matches!(constructor_behavior, crate::ConstructorBehavior::Allow);
  let proto = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    call_handler: None,
    natives: Vec::new(),
    parent_fn: ptr::null(),
  }));
  let instance = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    call_handler: None,
    natives: Vec::new(),
    parent_fn: ptr::null(),
  }));
  register_template(proto as usize, TemplKind::Obj);
  register_template(instance as usize, TemplKind::Obj);
  let t = Box::into_raw(Box::new(FnTemplate {
    callback,
    data: jsval(data_or_null),
    length,
    class_name: None,
    proto,
    instance,
    parent: ptr::null(),
    props: Vec::new(),
    constructable,
    cached_proto: ptr::null(),
    natives: Vec::new(),
  }));

  unsafe { (*instance).parent_fn = t };
  register_template(t as usize, TemplKind::Func);
  t as *const FunctionTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__GetFunction(
  this: *const FunctionTemplate,
  context: *const Context,
) -> *const Function {
  if this.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const FnTemplate) };
  let f = unsafe { make_function_len(ctx, t.callback, t.data, t.length) };

  if let Some(name) = &t.class_name {
    if let Ok(cname) = std::ffi::CString::new(name.as_str()) {
      let nameval = unsafe {
        let s = JSStringCreateWithUTF8CString(cname.as_ptr());
        let v = JSValueMakeString(ctx, s);
        JSStringRelease(s);
        v
      };
      // Force an own `name` — a plain set can't shadow the inherited
      // non-writable `Function.prototype.name` (see `v8__Function__SetName`).
      unsafe { define_own_name(ctx, f, nameval) };
    }
  }

  apply_props(ctx, f, &t.props);
  apply_natives(ctx, f, &t.natives);

  if t.constructable {
    let proto_obj =
      unsafe { build_prototype_object(ctx, this as *const FnTemplate) };
    let key = unsafe { cached_str(&STR_PROTOTYPE, c"prototype".as_ptr()) };
    let mut exc: JSValueRef = ptr::null();
    unsafe {
      JSObjectSetProperty(ctx, f, key, proto_obj as JSValueRef, 0, &mut exc);
    }
  }

  intern_ctx::<Function>(ctx, f as JSValueRef)
}

unsafe fn build_prototype_object(
  ctx: JSContextRef,
  tp: *const FnTemplate,
) -> JSObjectRef {
  let t = unsafe { &mut *(tp as *mut FnTemplate) };
  if !t.cached_proto.is_null() {
    return t.cached_proto as JSObjectRef;
  }
  let proto_obj =
    unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  let proto = unsafe { &*t.proto };
  if !proto.props.is_empty() {
    apply_props(ctx, proto_obj, &proto.props);
  }
  apply_accessors(ctx, proto_obj, &proto.accessors);
  apply_natives(ctx, proto_obj, &proto.natives);

  if !t.parent.is_null() {
    let parent_proto = unsafe { build_prototype_object(ctx, t.parent) };
    unsafe { JSObjectSetPrototype(ctx, proto_obj, parent_proto as JSValueRef) };
  }

  let gctx = unsafe { JSContextGetGlobalContext(ctx) };
  unsafe { JSValueProtect(gctx, proto_obj as JSValueRef) };
  t.cached_proto = proto_obj as JSValueRef;
  proto_obj
}

fn apply_props(
  ctx: JSContextRef,
  obj: JSObjectRef,
  props: &[(JSValueRef, JSValueRef, u32)],
) {
  for &(key, value, attr) in props {
    if key.is_null() {
      continue;
    }
    let mut exc: JSValueRef = ptr::null();
    let keystr = unsafe { JSValueToStringCopy(ctx, key, &mut exc) };
    if keystr.is_null() {
      continue;
    }

    let value = materialize_template_value(ctx, value);
    unsafe {
      JSObjectSetProperty(ctx, obj, keystr, value, attr, &mut exc);
      JSStringRelease(keystr);
    }
  }
}

fn materialize_template_value(
  ctx: JSContextRef,
  value: JSValueRef,
) -> JSValueRef {
  if value.is_null() {
    return value;
  }
  let raw = value as *const c_void as usize;
  let kind = TEMPLATES.with(|t| t.borrow().get(&raw).copied());
  match kind {
    Some(TemplKind::Func) => {
      let f = v8__FunctionTemplate__GetFunction(
        value as *const FunctionTemplate,
        ctx as *const Context,
      );
      if f.is_null() { value } else { jsval(f) }
    }
    Some(TemplKind::Obj) => {
      let o = v8__ObjectTemplate__NewInstance(
        value as *const ObjectTemplate,
        ctx as *const Context,
      );
      if o.is_null() { value } else { jsval(o) }
    }
    Some(TemplKind::Dict) | None => value,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__Inherit(
  this: *const FunctionTemplate,
  parent: *const FunctionTemplate,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  t.parent = parent as *const FnTemplate;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__PrototypeTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const FnTemplate) };
  t.proto as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__InstanceTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const FnTemplate) };
  t.instance as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetClassName(
  this: *const FunctionTemplate,
  name: *const String,
) {
  if this.is_null() || name.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let mut exc: JSValueRef = ptr::null();
  let s = unsafe { JSValueToStringCopy(ctx, jsval(name), &mut exc) };
  if s.is_null() {
    return;
  }
  let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
  let mut buf = vec![0u8; max];
  let n =
    unsafe { JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max) };
  unsafe { JSStringRelease(s) };
  if n > 0 {
    buf.truncate(n - 1);
    if let Ok(name) = std::string::String::from_utf8(buf) {
      t.class_name = Some(name);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__New(
  isolate: *mut RealIsolate,
  templ: *const FunctionTemplate,
) -> *const ObjectTemplate {
  let _ = (isolate, templ);
  let t = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    call_handler: None,
    natives: Vec::new(),
    parent_fn: ptr::null(),
  }));
  register_template(t as usize, TemplKind::Obj);
  t as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__NewInstance(
  this: *const ObjectTemplate,
  context: *const Context,
) -> *const Object {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  // A call handler makes the instance a function object. JSC cannot add a
  // [[Call]] slot to an ordinary object after the fact, so the instance is
  // built as one from the start; the property and accessor installation below
  // is the same for both shapes.
  let call_handler = if this.is_null() {
    None
  } else {
    unsafe { &*(this as *const ObjTemplate) }.call_handler
  };
  let obj = match call_handler {
    Some((callback, data)) => unsafe {
      make_function_len(ctx, callback, data, 0)
    },
    None => unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) },
  };
  if !this.is_null() {
    let t = unsafe { &*(this as *const ObjTemplate) };

    if !t.parent_fn.is_null() {
      let proto_obj = unsafe { build_prototype_object(ctx, t.parent_fn) };
      unsafe { JSObjectSetPrototype(ctx, obj, proto_obj as JSValueRef) };
    }

    apply_props(ctx, obj, &t.props);
    apply_accessors(ctx, obj, &t.accessors);
    apply_natives(ctx, obj, &t.natives);
  }
  intern_ctx::<Object>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetAccessorProperty(
  this: *const ObjectTemplate,
  key: *const Name,
  getter: *const FunctionTemplate,
  setter: *const FunctionTemplate,
  attr: PropertyAttribute,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.accessors.push(TemplAccessor {
    key: jsval(key),
    getter: getter as *const FnTemplate,
    setter: setter as *const FnTemplate,
    attr: attr.as_u32_lenient(),
  });
}

fn apply_accessors(
  ctx: JSContextRef,
  obj: JSObjectRef,
  accessors: &[TemplAccessor],
) {
  if accessors.is_empty() {
    return;
  }
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let okey = JSStringCreateWithUTF8CString(c"Object".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let object_ctor = JSObjectGetProperty(ctx, global, okey, &mut exc);
    JSStringRelease(okey);
    if object_ctor.is_null() || !JSValueIsObject(ctx, object_ctor) {
      return;
    }
    let dpkey = JSStringCreateWithUTF8CString(c"defineProperty".as_ptr());
    let define =
      JSObjectGetProperty(ctx, object_ctor as JSObjectRef, dpkey, &mut exc);
    JSStringRelease(dpkey);
    if define.is_null() || !JSValueIsObject(ctx, define) {
      return;
    }

    let get_str = JSStringCreateWithUTF8CString(c"get".as_ptr());
    let set_str = JSStringCreateWithUTF8CString(c"set".as_ptr());
    let enum_str = JSStringCreateWithUTF8CString(c"enumerable".as_ptr());
    let conf_str = JSStringCreateWithUTF8CString(c"configurable".as_ptr());

    for acc in accessors {
      if acc.key.is_null() {
        continue;
      }
      let desc = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      if !acc.getter.is_null() {
        let gf = v8__FunctionTemplate__GetFunction(
          acc.getter as *const FunctionTemplate,
          ctx as *const Context,
        );
        if !gf.is_null() {
          JSObjectSetProperty(ctx, desc, get_str, jsval(gf), 0, &mut exc);
        }
      }
      if !acc.setter.is_null() {
        let sf = v8__FunctionTemplate__GetFunction(
          acc.setter as *const FunctionTemplate,
          ctx as *const Context,
        );
        if !sf.is_null() {
          JSObjectSetProperty(ctx, desc, set_str, jsval(sf), 0, &mut exc);
        }
      }

      let enumerable = (acc.attr & 2) == 0;
      let configurable = (acc.attr & 4) == 0;
      JSObjectSetProperty(
        ctx,
        desc,
        enum_str,
        JSValueMakeBoolean(ctx, enumerable),
        0,
        &mut exc,
      );
      JSObjectSetProperty(
        ctx,
        desc,
        conf_str,
        JSValueMakeBoolean(ctx, configurable),
        0,
        &mut exc,
      );

      let args = [obj as JSValueRef, acc.key, desc as JSValueRef];
      JSObjectCallAsFunction(
        ctx,
        define as JSObjectRef,
        object_ctor as JSObjectRef,
        3,
        args.as_ptr(),
        &mut exc,
      );
    }

    JSStringRelease(get_str);
    JSStringRelease(set_str);
    JSStringRelease(enum_str);
    JSStringRelease(conf_str);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetIndexedPropertyHandler(
  this: *const ObjectTemplate,
  getter: Option<crate::IndexedPropertyGetterCallback>,
  setter: Option<crate::IndexedPropertySetterCallback>,
  query: Option<crate::IndexedPropertyQueryCallback>,
  deleter: Option<crate::IndexedPropertyDeleterCallback>,
  enumerator: Option<crate::IndexedPropertyEnumeratorCallback>,
  definer: Option<crate::IndexedPropertyDefinerCallback>,
  descriptor: Option<crate::IndexedPropertyDescriptorCallback>,
  data_or_null: *const Value,
  flags: crate::PropertyHandlerFlags,
) {
  let _ = (
    this,
    getter,
    setter,
    query,
    deleter,
    enumerator,
    definer,
    descriptor,
    data_or_null,
    flags,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetInternalFieldCount(
  this: *const ObjectTemplate,
  value: crate::support::int,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.internal_field_count = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNamedPropertyHandler(
  this: *const ObjectTemplate,
  getter: Option<crate::NamedPropertyGetterCallback>,
  setter: Option<crate::NamedPropertySetterCallback>,
  query: Option<crate::NamedPropertyQueryCallback>,
  deleter: Option<crate::NamedPropertyDeleterCallback>,
  enumerator: Option<crate::NamedPropertyEnumeratorCallback>,
  definer: Option<crate::NamedPropertyDefinerCallback>,
  descriptor: Option<crate::NamedPropertyDescriptorCallback>,
  data_or_null: *const Value,
  flags: crate::PropertyHandlerFlags,
) {
  let _ = (
    this,
    getter,
    setter,
    query,
    deleter,
    enumerator,
    definer,
    descriptor,
    data_or_null,
    flags,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__NewTarget(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  if info.is_construct && !info.new_target.is_null() {
    return info.new_target as *const Value;
  }
  (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value
}

#[inline]
fn prop_cbinfo<'a>(this: *const c_void) -> Option<&'a mut PropCbInfo> {
  unsafe { (this as *mut PropCbInfo).as_mut() }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut RealIsolate {
  match prop_cbinfo(this) {
    Some(info) if !info.isolate.is_null() => info.isolate,
    _ => crate::jsc::core::current_iso(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetReturnValue(
  this: *const c_void,
) -> usize {
  match prop_cbinfo(this) {
    Some(info) => (&mut info.return_slot as *mut JSValueRef) as usize,
    None => 0,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Holder(
  this: *const c_void,
) -> *const Object {
  match prop_cbinfo(this) {
    Some(info) => intern_ctx::<Object>(info.ctx, info.holder),
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__ShouldThrowOnError(
  _this: *const c_void,
) -> bool {
  false
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__IsConstructCall(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetAccessorProperty(
  _this: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _getter: *const std::os::raw::c_void,
  _setter: *const std::os::raw::c_void,
  _attr: crate::PropertyAttribute,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetName(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptColumnNumber(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptLineNumber(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptOrigin(
  _this: *const std::os::raw::c_void,
  _out: *mut std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__ScriptId(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__InternalFieldCount(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetCallAsFunctionHandler(
  this: *const std::os::raw::c_void,
  callback: FunctionCallback,
  data_or_null: *const Value,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.call_handler = Some((callback, jsval(data_or_null)));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetImmutableProto(
  _this: *const std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNativeDataProperty(
  this: *const ObjectTemplate,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) {
  v8__Template__SetNativeDataProperty(
    this as *const crate::Template,
    key,
    getter,
    setter,
    data_or_null,
    attr,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Data(
  this: *const c_void,
) -> *const Value {
  match prop_cbinfo(this) {
    Some(info) => intern_ctx::<Value>(info.ctx, info.data),
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Get(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetEmptyString(
  _this: *mut std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Signature__New(
  _isolate: *mut std::os::raw::c_void,
  _templ: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetIntrinsicDataProperty(
  _this: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _intrinsic: crate::Intrinsic,
  _attr: crate::PropertyAttribute,
) {
}
