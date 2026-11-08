//! QuickJS-ng-backed shims for the "function" family:
//! Function / FunctionCallbackInfo / ReturnValue / Template / ObjectTemplate /
//! Signature / External.
//!
//! Ported from the JSC backend (`src/function.rs`) with JSC C-API calls
//! swapped for QuickJS-ng calls, and from the deno PR's QuickJS logic
//! (`reference/qjs_v8_compat/src/function.rs`, `external.rs`). The C-ABI shape
//! (the `RawFunctionCallbackInfoParts` / `RawReturnValue` layouts and the
//! `*const FunctionCallbackInfo` contract) is identical to the JSC backend — the
//! vendored `src/function.rs` only cares about layout, not which engine backs it.
//!
//! ## Host functions
//! QuickJS-ng's `JS_NewCFunctionData` is documented to crash in this build (see
//! the PR), so — exactly like the PR's `build_op_function` — we create callable
//! functions with `JS_NewCFunction2(..., JS_CFUNC_generic_magic, magic)` and
//! recover the (v8 callback, data) pair from a per-thread dispatch table keyed by
//! the integer `magic`. Constructor (`new F()`) dispatch reuses the same table
//! via a `JS_CFUNC_constructor_magic` trampoline.
//!
//! ## Refcount discipline
//! Every shim that RETURNS a v8 handle routes its `JSValue` through
//! `intern`/`intern_dup` so the arena owns exactly one refcount. The dispatch
//! table holds an owned (`JS_DupValue`'d) copy of each callback's `data`
//! JSValue; it is intentionally never freed (lives for the isolate's lifetime,
//! like a v8 FunctionTemplate's data).

#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
  push_callback_context,
};
use crate::quickjs::quickjs_sys::*;
use crate::{
  Array, Boolean, Context, Data, External, Function, FunctionCallback,
  FunctionCallbackInfo, FunctionTemplate, Integer, Intrinsic, Local, Name,
  Object, ObjectTemplate, PropertyAttribute, PropertyDescriptor, RealIsolate,
  Signature, String, Value,
};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;
use std::ptr::NonNull;
use std::slice;

unsafe extern "C" {

  fn JS_NewClassID(rt: *mut JSRuntime, pclass_id: *mut JSClassID) -> JSClassID;
  fn JS_NewClass(
    rt: *mut JSRuntime,
    class_id: JSClassID,
    class_def: *const JSClassDef,
  ) -> c_int;
  fn JS_NewObjectClass(ctx: *mut JSContext, class_id: c_int) -> JSValue;
  fn JS_SetOpaque(obj: JSValue, opaque: *mut c_void);
  fn JS_GetOpaque(obj: JSValue, class_id: JSClassID) -> *mut c_void;
  fn JS_GetAnyOpaque(obj: JSValue, class_id: *mut JSClassID) -> *mut c_void;
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto: JSValue,
  ) -> c_int;
  fn JS_GetPrototype(ctx: *mut JSContext, obj: JSValue) -> JSValue;
  fn JS_PreventExtensions(ctx: *mut JSContext, obj: JSValue) -> c_int;
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;

  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;

  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_DupAtom(ctx: *mut JSContext, v: JSAtom) -> JSAtom;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_DefinePropertyValue(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_DefineProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
    getter: JSValue,
    setter: JSValue,
    flags: c_int,
  ) -> c_int;
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
  fn JS_GetLength(ctx: *mut JSContext, obj: JSValue, pres: *mut i64) -> c_int;
  fn JS_AtomToCStringLen(
    ctx: *mut JSContext,
    plen: *mut usize,
    atom: JSAtom,
  ) -> *const c_char;
}

type JSClassFinalizer = unsafe extern "C" fn(rt: *mut JSRuntime, val: JSValue);

#[repr(C)]
struct JSPropertyDescriptorQjs {
  flags: c_int,
  value: JSValue,
  getter: JSValue,
  setter: JSValue,
}

#[repr(C)]
struct JSClassExoticMethods {
  get_own_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      desc: *mut JSPropertyDescriptorQjs,
      obj: JSValue,
      prop: JSAtom,
    ) -> c_int,
  >,
  get_own_property_names: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      ptab: *mut *mut JSPropertyEnum,
      plen: *mut u32,
      obj: JSValue,
    ) -> c_int,
  >,
  delete_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      obj: JSValue,
      prop: JSAtom,
    ) -> c_int,
  >,
  define_own_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      obj: JSValue,
      prop: JSAtom,
      val: JSValue,
      getter: JSValue,
      setter: JSValue,
      flags: c_int,
    ) -> c_int,
  >,
  has_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      obj: JSValue,
      prop: JSAtom,
    ) -> c_int,
  >,
  get_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      obj: JSValue,
      prop: JSAtom,
      receiver: JSValue,
    ) -> JSValue,
  >,
  set_property: Option<
    unsafe extern "C" fn(
      ctx: *mut JSContext,
      obj: JSValue,
      prop: JSAtom,
      value: JSValue,
      receiver: JSValue,
      flags: c_int,
    ) -> c_int,
  >,
}

#[repr(C)]
struct JSClassDef {
  class_name: *const c_char,
  finalizer: Option<JSClassFinalizer>,
  gc_mark: *const c_void,
  call: *const c_void,
  exotic: *const JSClassExoticMethods,
}

#[repr(C)]
pub(super) struct RawReturnValue(usize);

#[repr(C)]
pub(super) struct RawFunctionCallbackInfoParts {
  isolate: *mut RealIsolate,
  return_value: usize,
  data: *const Value,
  length: crate::support::int,
}

#[repr(C)]
struct CbInfo {
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  this: JSValue,
  data: JSValue,
  new_target: JSValue,
  is_construct: bool,
  args: Vec<JSValue>,

  return_slot: Box<JSValue>,
}

struct FnTemplate {
  callback: FunctionCallback,

  data: JSValue,

  constructable: bool,
  length: i32,
  class_name: Option<std::string::String>,
  proto: *mut ObjTemplate,
  instance: *mut ObjTemplate,
  parent: *const FnTemplate,
  signature: *const FnTemplate,

  props: Vec<(JSValue, JSValue, u32)>,
  accessors: Vec<TemplAccessor>,

  cached_proto: JSValue,
  fast_overloads: Vec<RawCFunction>,
}

struct TemplAccessor {
  key: JSValue,
  getter: *const FnTemplate,
  setter: *const FnTemplate,
  native_getter: Option<crate::AccessorNameGetterCallback>,
  native_setter: Option<crate::AccessorNameSetterCallback>,
  data: JSValue,
  attr: u32,
  /// A lazy data property: the native getter runs once, on first access, and
  /// its result replaces the accessor with a plain data property.
  lazy: bool,
}

struct NamedHandler {
  getter: Option<crate::NamedPropertyGetterCallback>,
  setter: Option<crate::NamedPropertySetterCallback>,
  query: Option<crate::NamedPropertyQueryCallback>,
  deleter: Option<crate::NamedPropertyDeleterCallback>,
  enumerator: Option<crate::NamedPropertyEnumeratorCallback>,
  definer: Option<crate::NamedPropertyDefinerCallback>,
  descriptor: Option<crate::NamedPropertyDescriptorCallback>,
  data: JSValue,
  owner_ctx: *mut JSContext,
  non_masking: bool,
  only_intercept_strings: bool,
}

struct IndexedHandler {
  getter: Option<crate::IndexedPropertyGetterCallback>,
  setter: Option<crate::IndexedPropertySetterCallback>,
  query: Option<crate::IndexedPropertyQueryCallback>,
  deleter: Option<crate::IndexedPropertyDeleterCallback>,
  enumerator: Option<crate::IndexedPropertyEnumeratorCallback>,
  definer: Option<crate::IndexedPropertyDefinerCallback>,
  descriptor: Option<crate::IndexedPropertyDescriptorCallback>,
  data: JSValue,
  owner_ctx: *mut JSContext,
  non_masking: bool,
}

struct NamedHandlerInstance {
  named_handler: Option<NamedHandler>,
  indexed_handler: Option<IndexedHandler>,
}

struct SignatureInfo {
  templ: *const FnTemplate,
}

struct ObjTemplate {
  state: UnsafeCell<ObjTemplateState>,
}

struct ObjTemplateState {
  internal_field_count: i32,
  props: Vec<(JSValue, JSValue, u32)>,
  accessors: Vec<TemplAccessor>,
  named_handler: Option<NamedHandler>,
  indexed_handler: Option<IndexedHandler>,
  immutable_proto: bool,

  /// Set by `SetCallAsFunctionHandler`: instances of this template are
  /// callable and invoke this callback. The `JSValue` is the handler's data,
  /// owned by the template.
  call_handler: Option<(FunctionCallback, JSValue)>,

  parent_fn: *const FnTemplate,
}

impl ObjTemplate {
  fn new(parent_fn: *const FnTemplate) -> Self {
    Self {
      state: UnsafeCell::new(ObjTemplateState {
        internal_field_count: 0,
        props: Vec::new(),
        accessors: Vec::new(),
        named_handler: None,
        indexed_handler: None,
        immutable_proto: false,
        call_handler: None,
        parent_fn,
      }),
    }
  }

  /// Bare template for tape replay.
  fn default_for_tape() -> ObjTemplate {
    Self::new(std::ptr::null())
  }

  unsafe fn state(&self) -> &ObjTemplateState {
    unsafe { &*self.state.get() }
  }

  unsafe fn state_mut(&self) -> &mut ObjTemplateState {
    unsafe { &mut *self.state.get() }
  }
}

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

const JS_GPN_STRING_MASK_QJS: c_int = 1 << 0;
const JS_PROP_HAS_CONFIGURABLE_QJS: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE_QJS: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE_QJS: c_int = 1 << 10;
const JS_PROP_HAS_GET_QJS: c_int = 1 << 11;
const JS_PROP_HAS_SET_QJS: c_int = 1 << 12;
const JS_PROP_HAS_VALUE_QJS: c_int = 1 << 13;
const JS_PROP_NO_EXOTIC_QJS: c_int = 1 << 17;

struct DispatchEntry {
  callback: FunctionCallback,

  data: JSValue,
  data_external: Option<*mut c_void>,
  data_is_undefined: bool,

  instance: *const ObjTemplate,
  signature: *const FnTemplate,
  fast_overloads: Vec<RawCFunction>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct ReusableDispatchKey {
  callback: usize,
  data_external: Option<usize>,
  data_is_undefined: bool,
}

struct DispatchTable {
  entries: Vec<DispatchEntry>,
  reusable: std::collections::HashMap<ReusableDispatchKey, c_int>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawCTypeInfo {
  type_: u8,
  flags_: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawCFunctionInfo {
  return_info_: RawCTypeInfo,
  repr_: u8,
  arg_count_: c_uint,
  arg_info_: *const RawCTypeInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawCFunction {
  address_: *const c_void,
  type_info_: *const RawCFunctionInfo,
}

const CTYPE_VOID: u8 = 0;
const CTYPE_BOOL: u8 = 1;
const CTYPE_UINT32: u8 = 4;
const CTYPE_UINT64: u8 = 6;
const CTYPE_FLOAT32: u8 = 7;
const CTYPE_FLOAT64: u8 = 8;
const CTYPE_POINTER: u8 = 9;
const CTYPE_V8_VALUE: u8 = 10;
const CTYPE_SEQ_ONE_BYTE_STRING: u8 = 11;
const CTYPE_CALLBACK_OPTIONS: u8 = 255;
const INT64_REPR_BIGINT: u8 = 1;

fn copy_fast_overloads(
  c_functions: *const crate::fast_api::CFunction,
  c_functions_len: usize,
) -> Vec<RawCFunction> {
  if c_functions.is_null() || c_functions_len == 0 {
    return Vec::new();
  }
  unsafe {
    slice::from_raw_parts(c_functions as *const RawCFunction, c_functions_len)
      .to_vec()
  }
}

thread_local! {
    static DISPATCH: std::cell::RefCell<DispatchTable> =
        std::cell::RefCell::new(DispatchTable {
          entries: Vec::new(),
          reusable: std::collections::HashMap::new(),
        });
    static SNAPSHOT_FUNCTIONS: std::cell::RefCell<
      std::collections::HashMap<usize, SnapshotFunctionInfo>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static NAMED_GETTER_DISPATCH: std::cell::RefCell<Vec<NamedGetterEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static GLOBAL_DEFINE_DISPATCH: std::cell::RefCell<Vec<*const GlobalDefineEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotFunctionInfo {
  pub callback: FunctionCallback,
  pub data_external: Option<*mut c_void>,
  pub length: i32,
  pub constructable: bool,
}

pub(crate) struct SnapshotFunctionTemplateInfo {
  pub callback: FunctionCallback,
  pub data_external: Option<*mut c_void>,
  pub length: i32,
  pub constructable: bool,
  pub class_name: Option<std::string::String>,
  pub cached_proto: Option<JSValue>,
  pub instance_internal_field_count: i32,
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotNamedHandlerInfo {
  pub getter: Option<crate::NamedPropertyGetterCallback>,
  pub setter: Option<crate::NamedPropertySetterCallback>,
  pub query: Option<crate::NamedPropertyQueryCallback>,
  pub deleter: Option<crate::NamedPropertyDeleterCallback>,
  pub enumerator: Option<crate::NamedPropertyEnumeratorCallback>,
  pub definer: Option<crate::NamedPropertyDefinerCallback>,
  pub descriptor: Option<crate::NamedPropertyDescriptorCallback>,
  pub data: JSValue,
  pub owner_ctx: *mut JSContext,
  pub non_masking: bool,
  pub only_intercept_strings: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotIndexedHandlerInfo {
  pub getter: Option<crate::IndexedPropertyGetterCallback>,
  pub setter: Option<crate::IndexedPropertySetterCallback>,
  pub query: Option<crate::IndexedPropertyQueryCallback>,
  pub deleter: Option<crate::IndexedPropertyDeleterCallback>,
  pub enumerator: Option<crate::IndexedPropertyEnumeratorCallback>,
  pub definer: Option<crate::IndexedPropertyDefinerCallback>,
  pub descriptor: Option<crate::IndexedPropertyDescriptorCallback>,
  pub data: JSValue,
  pub owner_ctx: *mut JSContext,
  pub non_masking: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct SnapshotObjectTemplateInfo {
  pub internal_field_count: i32,
  pub named_handler: Option<SnapshotNamedHandlerInfo>,
  pub indexed_handler: Option<SnapshotIndexedHandlerInfo>,
}

#[derive(Clone, Copy)]
struct NamedGetterEntry {
  getter: crate::NamedPropertyGetterCallback,
  data: JSValue,
  owner_ctx: *mut JSContext,
  atom: JSAtom,
}

struct GlobalDefineEntry {
  original: JSValue,
  object_ctor: JSValue,
  global: JSValue,
  handler: NamedHandler,
}

pub(crate) mod timing {
  use std::cell::Cell;
  use std::time::{Duration, Instant};
  thread_local! {
      pub static ON: Cell<bool> = Cell::new(std::env::var_os("V82JSC_TIMING").is_some());
      pub static GETFN_N: Cell<u64> = const { Cell::new(0) };
      pub static GETFN_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
      pub static PROTO_N: Cell<u64> = const { Cell::new(0) };
      pub static PROTO_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
      pub static APPLY_N: Cell<u64> = const { Cell::new(0) };
      pub static NEWINST_N: Cell<u64> = const { Cell::new(0) };
      pub static NEWINST_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
  }
  #[inline]
  pub fn on() -> bool {
    ON.with(|c| c.get())
  }
  pub fn now() -> Option<Instant> {
    if on() { Some(Instant::now()) } else { None }
  }
  pub fn add(
    n: &'static std::thread::LocalKey<Cell<u64>>,
    t: &'static std::thread::LocalKey<Cell<Duration>>,
    start: Option<Instant>,
  ) {
    if let Some(s) = start {
      n.with(|c| c.set(c.get() + 1));
      t.with(|c| c.set(c.get() + s.elapsed()));
    }
  }
  pub fn dump() {
    if !on() {
      return;
    }
    let g = (GETFN_N.with(|c| c.get()), GETFN_T.with(|c| c.get()));
    let p = (PROTO_N.with(|c| c.get()), PROTO_T.with(|c| c.get()));
    let a = APPLY_N.with(|c| c.get());
    let ni = (NEWINST_N.with(|c| c.get()), NEWINST_T.with(|c| c.get()));
    eprintln!(
      "[V82JSC_TIMING] GetFunction: {} calls, {:.2} ms",
      g.0,
      g.1.as_secs_f64() * 1000.0
    );
    eprintln!(
      "[V82JSC_TIMING] build_prototype_object: {} calls, {:.2} ms",
      p.0,
      p.1.as_secs_f64() * 1000.0
    );
    eprintln!(
      "[V82JSC_TIMING] ObjectTemplate::NewInstance: {} calls, {:.2} ms",
      ni.0,
      ni.1.as_secs_f64() * 1000.0
    );
    eprintln!("[V82JSC_TIMING] apply_props: {} calls", a);
  }
}

fn register_dispatch(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  data_external: Option<*mut c_void>,
  data_is_undefined: bool,
  instance: *const ObjTemplate,
  signature: *const FnTemplate,
  fast_overloads: Vec<RawCFunction>,
) -> c_int {
  DISPATCH.with(|t| {
    let mut t = t.borrow_mut();
    let reusable_key = (instance.is_null()
      && signature.is_null()
      && fast_overloads.is_empty()
      && (data_is_undefined || data_external.is_some()))
    .then_some(ReusableDispatchKey {
      callback: callback as usize,
      data_external: data_external.map(|data| data as usize),
      data_is_undefined,
    });
    if let Some(key) = reusable_key
      && let Some(&index) = t.reusable.get(&key)
    {
      unsafe { JS_FreeValue(ctx, data) };
      return index;
    }
    let idx = t.entries.len() as c_int;
    t.entries.push(DispatchEntry {
      callback,
      data,
      data_external,
      data_is_undefined,
      instance,
      signature,
      fast_overloads,
    });
    if let Some(key) = reusable_key {
      t.reusable.insert(key, idx);
    }
    idx
  })
}

fn lookup_dispatch(
  ctx: *mut JSContext,
  idx: c_int,
) -> Option<(
  FunctionCallback,
  JSValue,
  *const ObjTemplate,
  *const FnTemplate,
  Vec<RawCFunction>,
)> {
  DISPATCH.with(|t| {
    t.borrow().entries.get(idx as usize).map(|e| {
      // The dispatch table is process-global and outlives any single runtime;
      // reusable entries (undefined/External data) are shared across runtimes,
      // including by snapshot-restored functions. The stored JSValue may
      // therefore belong to a dead runtime — materialize the callback data in
      // the calling context instead of handing out the stored value. The
      // returned value is OWNED by the caller (free after dispatch).
      let data = if e.data_is_undefined {
        jsv_undefined()
      } else if let Some(ptr) = e.data_external {
        make_external_jsvalue(super::core::current_iso(), ctx, ptr)
      } else {
        unsafe { JS_DupValue(ctx, e.data) }
      };
      (
        e.callback,
        data,
        e.instance,
        e.signature,
        e.fast_overloads.clone(),
      )
    })
  })
}

#[cfg(test)]
pub(crate) fn dispatch_entry_count() -> usize {
  DISPATCH.with(|entries| entries.borrow().entries.len())
}

fn register_named_getter(
  ctx: *mut JSContext,
  handler: &NamedHandler,
  atom: JSAtom,
) -> c_int {
  let Some(getter) = handler.getter else {
    return 0;
  };
  NAMED_GETTER_DISPATCH.with(|d| {
    let mut d = d.borrow_mut();
    let idx = d.len() as c_int;
    d.push(NamedGetterEntry {
      getter,
      data: unsafe { JS_DupValue(ctx, handler.data) },
      owner_ctx: handler.owner_ctx,
      atom: unsafe { JS_DupAtom(ctx, atom) },
    });
    idx
  })
}

fn lookup_named_getter(idx: c_int) -> Option<NamedGetterEntry> {
  NAMED_GETTER_DISPATCH.with(|d| d.borrow().get(idx as usize).copied())
}

fn register_global_define(entry: GlobalDefineEntry) -> c_int {
  GLOBAL_DEFINE_DISPATCH.with(|d| {
    let mut d = d.borrow_mut();
    let idx = d.len() as c_int;
    d.push(Box::into_raw(Box::new(entry)));
    idx
  })
}

fn lookup_global_define(idx: c_int) -> Option<&'static GlobalDefineEntry> {
  GLOBAL_DEFINE_DISPATCH.with(|d| {
    d.borrow()
      .get(idx as usize)
      .copied()
      .filter(|ptr| !ptr.is_null())
      .map(|ptr| unsafe { &*ptr })
  })
}

unsafe fn materialize_named_handler_data(
  ctx: *mut JSContext,
  entry: NamedGetterEntry,
) -> (JSValue, bool) {
  if entry.owner_ctx.is_null() || entry.owner_ctx == ctx {
    return (entry.data, false);
  }

  let obj = unsafe { JS_NewObject(ctx) };
  let value =
    unsafe { JS_GetProperty(entry.owner_ctx, entry.data, entry.atom) };
  if value.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(entry.owner_ctx) };
    unsafe { JS_FreeValue(entry.owner_ctx, exc) };
    return (obj, true);
  }
  unsafe {
    JS_DefinePropertyValue(
      ctx,
      obj,
      entry.atom,
      value,
      JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE | JS_PROP_WRITABLE,
    );
  }
  (obj, true)
}

#[inline]
fn intercepted_yes<T>(v: &T) -> bool {
  let raw = unsafe { std::mem::transmute_copy::<_, u32>(v) };
  raw == 0
}

fn clone_named_handler(
  ctx: *mut JSContext,
  handler: &NamedHandler,
) -> NamedHandler {
  let data = if jsv_is_undefined(&handler.data) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, handler.data) }
  };
  NamedHandler {
    getter: handler.getter,
    setter: handler.setter,
    query: handler.query,
    deleter: handler.deleter,
    enumerator: handler.enumerator,
    definer: handler.definer,
    descriptor: handler.descriptor,
    data,
    owner_ctx: handler.owner_ctx,
    non_masking: handler.non_masking,
    only_intercept_strings: handler.only_intercept_strings,
  }
}

fn clone_indexed_handler(
  ctx: *mut JSContext,
  handler: &IndexedHandler,
) -> IndexedHandler {
  let data = if jsv_is_undefined(&handler.data) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, handler.data) }
  };
  IndexedHandler {
    getter: handler.getter,
    setter: handler.setter,
    query: handler.query,
    deleter: handler.deleter,
    enumerator: handler.enumerator,
    definer: handler.definer,
    descriptor: handler.descriptor,
    data,
    owner_ctx: handler.owner_ctx,
    non_masking: handler.non_masking,
  }
}

fn new_object_for_template(
  ctx: *mut JSContext,
  templ: &ObjTemplateState,
) -> JSValue {
  // A call handler makes the instance a function object. QuickJS has no way to
  // add a [[Call]] slot to an existing ordinary object, so the instance is
  // built as one from the start; property and accessor installation afterwards
  // is the same for both shapes. Interceptors need the exotic class instead,
  // and the two cannot be combined here.
  if let Some((callback, data)) = templ.call_handler {
    return unsafe {
      make_function_len_with_instance(
        ctx,
        callback,
        data,
        0,
        true,
        ptr::null(),
        ptr::null(),
        Vec::new(),
      )
    };
  }
  if templ.named_handler.is_none() && templ.indexed_handler.is_none() {
    return unsafe { JS_NewObject(ctx) };
  }
  let cid = named_handler_class_id_current();
  if cid == 0 {
    return unsafe { JS_NewObject(ctx) };
  }
  let obj = unsafe { JS_NewObjectClass(ctx, cid as c_int) };
  if jsv_is_exception(&obj) {
    return obj;
  }
  let inst = Box::new(NamedHandlerInstance {
    named_handler: templ
      .named_handler
      .as_ref()
      .map(|handler| clone_named_handler(ctx, handler)),
    indexed_handler: templ
      .indexed_handler
      .as_ref()
      .map(|handler| clone_indexed_handler(ctx, handler)),
  });
  unsafe { JS_SetOpaque(obj, Box::into_raw(inst) as *mut c_void) };
  obj
}

fn named_handler_class_id_current() -> JSClassID {
  let iso = current_iso();
  if iso.is_null() {
    return 0;
  }
  iso_state(iso).named_handler_class_id
}

unsafe fn named_handler_from_obj<'a>(
  obj: JSValue,
) -> Option<&'a mut NamedHandlerInstance> {
  let cid = named_handler_class_id_current();
  if cid == 0 {
    return None;
  }
  let ptr = unsafe { JS_GetOpaque(obj, cid) as *mut NamedHandlerInstance };
  if ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *ptr })
  }
}

pub(crate) fn is_named_handler_object(obj: JSValue) -> bool {
  let class_id = named_handler_class_id_current();
  class_id != 0 && !unsafe { JS_GetOpaque(obj, class_id) }.is_null()
}

unsafe fn atom_to_u32_index(ctx: *mut JSContext, atom: JSAtom) -> Option<u32> {
  let mut len = 0_usize;
  let ptr = unsafe { JS_AtomToCStringLen(ctx, &mut len, atom) };
  if ptr.is_null() || len == 0 {
    if !ptr.is_null() {
      unsafe { JS_FreeCString(ctx, ptr) };
    }
    return None;
  }

  let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, len) };
  let mut out = 0_u64;
  let mut ok = true;
  if len > 1 && bytes[0] == b'0' {
    ok = false;
  }
  if ok {
    for &b in bytes {
      if !b.is_ascii_digit() {
        ok = false;
        break;
      }
      out = out * 10 + u64::from(b - b'0');
      if out >= u64::from(u32::MAX) {
        ok = false;
        break;
      }
    }
  }
  unsafe { JS_FreeCString(ctx, ptr) };
  if ok { Some(out as u32) } else { None }
}

unsafe fn restore_callback_handles(iso: *mut RealIsolate, saved_depth: usize) {
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  while st.handles.len() > saved_depth {
    if let Some(slot) = st.handles.pop() {
      unsafe {
        JS_FreeValue(st.ctx, *slot);
        drop(Box::from_raw(slot));
      }
    }
  }
}

unsafe fn call_named_getter(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  handler: &NamedHandler,
  getter: crate::NamedPropertyGetterCallback,
) -> (c_int, JSValue) {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Value>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return (0, jsv_undefined());
  };
  let intercepted =
    unsafe { (getter)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    (-1, jsv_exception())
  } else if intercepted_yes(&intercepted) {
    let value = if jsv_is_undefined(&ret) {
      jsv_undefined()
    } else {
      unsafe { JS_DupValue(ctx, ret) }
    };
    (1, value)
  } else {
    (0, jsv_undefined())
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_named_query(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  handler: &NamedHandler,
  query: crate::NamedPropertyQueryCallback,
) -> (c_int, c_int) {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Integer>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return (0, 0);
  };
  let intercepted =
    unsafe { (query)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let mut attr = 0;
  let result = if unsafe { JS_HasException(ctx) } {
    (-1, 0)
  } else if intercepted_yes(&intercepted) {
    if !jsv_is_undefined(&ret) && unsafe { JS_ToInt32(ctx, &mut attr, ret) } < 0
    {
      (-1, 0)
    } else {
      (1, attr)
    }
  } else {
    (0, 0)
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_named_setter(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  value: JSValue,
  handler: &NamedHandler,
  setter: crate::NamedPropertySetterCallback,
) -> c_int {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let value_handle = intern_dup::<Value>(ctx, value);
  let (Some(key_handle), Some(value_handle)) = (
    NonNull::new(key_handle as *mut Name),
    NonNull::new(value_handle as *mut Value),
  ) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return 0;
  };
  let intercepted = unsafe {
    (setter)(
      crate::SealedLocal(key_handle),
      crate::SealedLocal(value_handle),
      prop_info,
    )
  };

  let _info = unsafe { Box::from_raw(info_ptr) };
  let result = if unsafe { JS_HasException(ctx) } {
    -1
  } else if intercepted_yes(&intercepted) {
    1
  } else {
    0
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_named_deleter(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  handler: &NamedHandler,
  deleter: crate::NamedPropertyDeleterCallback,
) -> c_int {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return 0;
  };
  let intercepted =
    unsafe { (deleter)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    -1
  } else if intercepted_yes(&intercepted) {
    if jsv_is_undefined(&ret) {
      1
    } else {
      unsafe { JS_ToBool(ctx, ret) }
    }
  } else {
    0
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_named_enumerator(
  ctx: *mut JSContext,
  this_val: JSValue,
  handler: &NamedHandler,
  enumerator: crate::NamedPropertyEnumeratorCallback,
) -> JSValue {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Array>;

  unsafe { (enumerator)(prop_info) };
  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if jsv_is_undefined(&ret) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, ret) }
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn construct_property_descriptor_for_callback(
  ctx: *mut JSContext,
  out: *mut PropertyDescriptor,
  val: JSValue,
  getter: JSValue,
  setter: JSValue,
  flags: c_int,
) {
  if flags & JS_PROP_HAS_GET_QJS != 0 || flags & JS_PROP_HAS_SET_QJS != 0 {
    let get_handle = intern_dup::<Value>(ctx, getter);
    let set_handle = intern_dup::<Value>(ctx, setter);
    unsafe {
      super::property::v8__PropertyDescriptor__CONSTRUCT__Get_Set(
        out, get_handle, set_handle,
      );
    }
  } else if flags & JS_PROP_HAS_VALUE_QJS != 0 {
    let value_handle = intern_dup::<Value>(ctx, val);
    if flags & JS_PROP_HAS_WRITABLE_QJS != 0 {
      unsafe {
        super::property::v8__PropertyDescriptor__CONSTRUCT__Value_Writable(
          out,
          value_handle,
          flags & JS_PROP_WRITABLE != 0,
        );
      }
    } else {
      unsafe {
        super::property::v8__PropertyDescriptor__CONSTRUCT__Value(
          out,
          value_handle,
        );
      }
    }
  } else {
    unsafe { super::property::v8__PropertyDescriptor__CONSTRUCT(out) };
  }

  if flags & JS_PROP_HAS_ENUMERABLE_QJS != 0 {
    unsafe {
      super::property::v8__PropertyDescriptor__set_enumerable(
        out,
        flags & JS_PROP_ENUMERABLE != 0,
      );
    }
  }
  if flags & JS_PROP_HAS_CONFIGURABLE_QJS != 0 {
    unsafe {
      super::property::v8__PropertyDescriptor__set_configurable(
        out,
        flags & JS_PROP_CONFIGURABLE != 0,
      );
    }
  }
}

unsafe fn call_named_definer(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  val: JSValue,
  getter: JSValue,
  setter: JSValue,
  flags: c_int,
  handler: &NamedHandler,
  definer: crate::NamedPropertyDefinerCallback,
) -> c_int {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return 0;
  };
  let mut pd = MaybeUninit::<PropertyDescriptor>::zeroed();
  unsafe {
    construct_property_descriptor_for_callback(
      ctx,
      pd.as_mut_ptr(),
      val,
      getter,
      setter,
      flags,
    )
  };
  let intercepted = unsafe {
    (definer)(crate::SealedLocal(key_handle), pd.as_ptr(), prop_info)
  };
  unsafe { super::property::v8__PropertyDescriptor__DESTRUCT(pd.as_mut_ptr()) };

  let _info = unsafe { Box::from_raw(info_ptr) };
  let result = if unsafe { JS_HasException(ctx) } {
    -1
  } else if intercepted_yes(&intercepted) {
    1
  } else {
    0
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_getter(
  ctx: *mut JSContext,
  this_val: JSValue,
  index: u32,
  handler: &IndexedHandler,
  getter: crate::IndexedPropertyGetterCallback,
) -> (c_int, JSValue) {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Value>;

  let intercepted = unsafe { (getter)(index, prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    (-1, jsv_exception())
  } else if intercepted_yes(&intercepted) {
    let value = if jsv_is_undefined(&ret) {
      jsv_undefined()
    } else {
      unsafe { JS_DupValue(ctx, ret) }
    };
    (1, value)
  } else {
    (0, jsv_undefined())
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_query(
  ctx: *mut JSContext,
  this_val: JSValue,
  index: u32,
  handler: &IndexedHandler,
  query: crate::IndexedPropertyQueryCallback,
) -> (c_int, c_int) {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Integer>;

  let intercepted = unsafe { (query)(index, prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let mut attr = 0;
  let result = if unsafe { JS_HasException(ctx) } {
    (-1, 0)
  } else if intercepted_yes(&intercepted) {
    if !jsv_is_undefined(&ret) && unsafe { JS_ToInt32(ctx, &mut attr, ret) } < 0
    {
      (-1, 0)
    } else {
      (1, attr)
    }
  } else {
    (0, 0)
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_setter(
  ctx: *mut JSContext,
  this_val: JSValue,
  index: u32,
  value: JSValue,
  handler: &IndexedHandler,
  setter: crate::IndexedPropertySetterCallback,
) -> c_int {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let value_handle = intern_dup::<Value>(ctx, value);
  let Some(value_handle) = NonNull::new(value_handle as *mut Value) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    unsafe { restore_callback_handles(iso, saved_depth) };
    return 0;
  };
  let intercepted =
    unsafe { (setter)(index, crate::SealedLocal(value_handle), prop_info) };

  let _info = unsafe { Box::from_raw(info_ptr) };
  let result = if unsafe { JS_HasException(ctx) } {
    -1
  } else if intercepted_yes(&intercepted) {
    1
  } else {
    0
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_deleter(
  ctx: *mut JSContext,
  this_val: JSValue,
  index: u32,
  handler: &IndexedHandler,
  deleter: crate::IndexedPropertyDeleterCallback,
) -> (c_int, c_int) {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let intercepted = unsafe { (deleter)(index, prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    (-1, 0)
  } else if intercepted_yes(&intercepted) {
    let ok = if jsv_is_undefined(&ret) {
      1
    } else {
      unsafe { JS_ToBool(ctx, ret) }
    };
    (1, ok)
  } else {
    (0, 1)
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_enumerator(
  ctx: *mut JSContext,
  this_val: JSValue,
  handler: &IndexedHandler,
  enumerator: crate::IndexedPropertyEnumeratorCallback,
) -> JSValue {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Array>;

  unsafe { (enumerator)(prop_info) };
  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if jsv_is_undefined(&ret) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, ret) }
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

unsafe fn call_indexed_definer(
  ctx: *mut JSContext,
  this_val: JSValue,
  index: u32,
  val: JSValue,
  getter: JSValue,
  setter: JSValue,
  flags: c_int,
  handler: &IndexedHandler,
  definer: crate::IndexedPropertyDefinerCallback,
) -> c_int {
  let _context_guard = push_callback_context(ctx);
  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: handler.data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Boolean>;

  let mut pd = MaybeUninit::<PropertyDescriptor>::zeroed();
  unsafe {
    construct_property_descriptor_for_callback(
      ctx,
      pd.as_mut_ptr(),
      val,
      getter,
      setter,
      flags,
    )
  };
  let intercepted = unsafe { (definer)(index, pd.as_ptr(), prop_info) };
  unsafe { super::property::v8__PropertyDescriptor__DESTRUCT(pd.as_mut_ptr()) };

  let _info = unsafe { Box::from_raw(info_ptr) };
  let result = if unsafe { JS_HasException(ctx) } {
    -1
  } else if intercepted_yes(&intercepted) {
    1
  } else {
    0
  };
  unsafe { restore_callback_handles(iso, saved_depth) };
  result
}

fn attr_to_descriptor_flags(attr: c_int) -> c_int {
  let mut flags = 0;
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

unsafe fn fill_desc_from_query(
  ctx: *mut JSContext,
  desc: *mut JSPropertyDescriptorQjs,
  attr: c_int,
) {
  if desc.is_null() {
    return;
  }
  unsafe {
    (*desc).flags = attr_to_descriptor_flags(attr);
    (*desc).value = jsv_undefined();
    (*desc).getter = jsv_undefined();
    (*desc).setter = jsv_undefined();
  }
  let _ = ctx;
}

unsafe fn fill_desc_from_descriptor_object(
  ctx: *mut JSContext,
  desc: *mut JSPropertyDescriptorQjs,
  obj: JSValue,
) -> c_int {
  if desc.is_null() {
    return 1;
  }
  let value = unsafe { JS_GetPropertyStr(ctx, obj, c"value".as_ptr()) };
  if jsv_is_exception(&value) {
    return -1;
  }
  let enumerable =
    unsafe { JS_GetPropertyStr(ctx, obj, c"enumerable".as_ptr()) };
  if jsv_is_exception(&enumerable) {
    unsafe { JS_FreeValue(ctx, value) };
    return -1;
  }
  let configurable =
    unsafe { JS_GetPropertyStr(ctx, obj, c"configurable".as_ptr()) };
  if jsv_is_exception(&configurable) {
    unsafe {
      JS_FreeValue(ctx, value);
      JS_FreeValue(ctx, enumerable);
    }
    return -1;
  }
  let writable = unsafe { JS_GetPropertyStr(ctx, obj, c"writable".as_ptr()) };
  if jsv_is_exception(&writable) {
    unsafe {
      JS_FreeValue(ctx, value);
      JS_FreeValue(ctx, enumerable);
      JS_FreeValue(ctx, configurable);
    }
    return -1;
  }

  let mut flags = 0;
  if unsafe { JS_ToBool(ctx, enumerable) } != 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if unsafe { JS_ToBool(ctx, configurable) } != 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  if unsafe { JS_ToBool(ctx, writable) } != 0 {
    flags |= JS_PROP_WRITABLE;
  }
  unsafe {
    JS_FreeValue(ctx, enumerable);
    JS_FreeValue(ctx, configurable);
    JS_FreeValue(ctx, writable);
    (*desc).flags = flags;
    (*desc).value = value;
    (*desc).getter = jsv_undefined();
    (*desc).setter = jsv_undefined();
  }
  1
}

unsafe fn enumerator_contains_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
  handler: &NamedHandler,
  enumerator: crate::NamedPropertyEnumeratorCallback,
) -> c_int {
  let arr = unsafe { call_named_enumerator(ctx, obj, handler, enumerator) };
  if jsv_is_exception(&arr) {
    return -1;
  }
  if jsv_is_undefined(&arr) {
    return 0;
  }
  let mut len: i64 = 0;
  if unsafe { JS_GetLength(ctx, arr, &mut len) } < 0 || len <= 0 {
    unsafe { JS_FreeValue(ctx, arr) };
    return 0;
  }
  let mut found = false;
  for i in 0..(len.min(u32::MAX as i64) as u32) {
    let item = unsafe { JS_GetPropertyUint32(ctx, arr, i) };
    if jsv_is_exception(&item) {
      unsafe { JS_FreeValue(ctx, arr) };
      return -1;
    }
    let atom = unsafe { JS_ValueToAtom(ctx, item) };
    unsafe { JS_FreeValue(ctx, item) };
    if atom == prop {
      found = true;
    }
    if atom != 0 {
      unsafe { JS_FreeAtom(ctx, atom) };
    }
    if found {
      break;
    }
  }
  unsafe { JS_FreeValue(ctx, arr) };
  if found { 1 } else { 0 }
}

unsafe fn indexed_enumerator_contains_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
  handler: &IndexedHandler,
  enumerator: crate::IndexedPropertyEnumeratorCallback,
) -> c_int {
  let arr = unsafe { call_indexed_enumerator(ctx, obj, handler, enumerator) };
  if jsv_is_exception(&arr) {
    return -1;
  }
  if jsv_is_undefined(&arr) {
    return 0;
  }
  let mut len: i64 = 0;
  if unsafe { JS_GetLength(ctx, arr, &mut len) } < 0 || len <= 0 {
    unsafe { JS_FreeValue(ctx, arr) };
    return 0;
  }
  let mut found = false;
  for i in 0..(len.min(u32::MAX as i64) as u32) {
    let item = unsafe { JS_GetPropertyUint32(ctx, arr, i) };
    if jsv_is_exception(&item) {
      unsafe { JS_FreeValue(ctx, arr) };
      return -1;
    }
    let atom = unsafe { JS_ValueToAtom(ctx, item) };
    unsafe { JS_FreeValue(ctx, item) };
    if atom == prop {
      found = true;
    }
    if atom != 0 {
      unsafe { JS_FreeAtom(ctx, atom) };
    }
    if found {
      break;
    }
  }
  unsafe { JS_FreeValue(ctx, arr) };
  if found { 1 } else { 0 }
}

unsafe extern "C" fn named_handler_get_own_property(
  ctx: *mut JSContext,
  desc: *mut JSPropertyDescriptorQjs,
  obj: JSValue,
  prop: JSAtom,
) -> c_int {
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return 0;
  };
  if let (Some(index), Some(handler)) = (
    unsafe { atom_to_u32_index(ctx, prop) },
    inst.indexed_handler.as_ref(),
  ) {
    if let Some(descriptor) = handler.descriptor {
      let (state, value) =
        unsafe { call_indexed_getter(ctx, obj, index, handler, descriptor) };
      if state < 0 {
        return -1;
      }
      if state > 0 {
        let rc = unsafe { fill_desc_from_descriptor_object(ctx, desc, value) };
        unsafe { JS_FreeValue(ctx, value) };
        return rc;
      }
    }

    if let Some(query) = handler.query {
      let (state, attr) =
        unsafe { call_indexed_query(ctx, obj, index, handler, query) };
      if state < 0 {
        return -1;
      }
      if state > 0 {
        unsafe { fill_desc_from_query(ctx, desc, attr) };
        return 1;
      }
    }

    if let Some(enumerator) = handler.enumerator {
      let state = unsafe {
        indexed_enumerator_contains_property(
          ctx, obj, prop, handler, enumerator,
        )
      };
      if state < 0 {
        return -1;
      }
      if state > 0 {
        if !desc.is_null() {
          unsafe {
            (*desc).flags =
              JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE | JS_PROP_WRITABLE;
            (*desc).value = jsv_undefined();
            (*desc).getter = jsv_undefined();
            (*desc).setter = jsv_undefined();
          }
        }
        return 1;
      }
    }
  }

  let Some(handler) = inst.named_handler.as_ref() else {
    return 0;
  };

  if let Some(descriptor) = handler.descriptor {
    let (state, value) =
      unsafe { call_named_getter(ctx, obj, prop, handler, descriptor) };
    if state < 0 {
      return -1;
    }
    if state > 0 {
      let rc = unsafe { fill_desc_from_descriptor_object(ctx, desc, value) };
      unsafe { JS_FreeValue(ctx, value) };
      return rc;
    }
  }

  if let Some(query) = handler.query {
    let (state, attr) =
      unsafe { call_named_query(ctx, obj, prop, handler, query) };
    if state < 0 {
      return -1;
    }
    if state > 0 {
      unsafe { fill_desc_from_query(ctx, desc, attr) };
      return 1;
    }
  }

  if let Some(enumerator) = handler.enumerator {
    let state = unsafe {
      enumerator_contains_property(ctx, obj, prop, handler, enumerator)
    };
    if state < 0 {
      return -1;
    }
    if state > 0 {
      if !desc.is_null() {
        unsafe {
          (*desc).flags =
            JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE | JS_PROP_WRITABLE;
          (*desc).value = jsv_undefined();
          (*desc).getter = jsv_undefined();
          (*desc).setter = jsv_undefined();
        }
      }
      return 1;
    }
  }

  0
}

unsafe extern "C" fn named_handler_get_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
  receiver: JSValue,
) -> JSValue {
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return jsv_undefined();
  };

  if let (Some(index), Some(handler)) = (
    unsafe { atom_to_u32_index(ctx, prop) },
    inst.indexed_handler.as_ref(),
  ) {
    if let Some(getter) = handler.getter {
      let (state, value) =
        unsafe { call_indexed_getter(ctx, receiver, index, handler, getter) };
      if state < 0 {
        return jsv_exception();
      }
      if state > 0 {
        return value;
      }
    }
  }

  if let Some(handler) = inst.named_handler.as_ref() {
    if let Some(getter) = handler.getter {
      let (state, value) =
        unsafe { call_named_getter(ctx, receiver, prop, handler, getter) };
      if state < 0 {
        return jsv_exception();
      }
      if state > 0 {
        return value;
      }
    }
  }

  jsv_undefined()
}

unsafe extern "C" fn named_handler_set_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
  value: JSValue,
  receiver: JSValue,
  _flags: c_int,
) -> c_int {
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return 0;
  };

  if let (Some(index), Some(handler)) = (
    unsafe { atom_to_u32_index(ctx, prop) },
    inst.indexed_handler.as_ref(),
  ) {
    if let Some(setter) = handler.setter {
      let state = unsafe {
        call_indexed_setter(ctx, receiver, index, value, handler, setter)
      };
      if state != 0 {
        return state;
      }
    }
  }

  if let Some(handler) = inst.named_handler.as_ref() {
    if let Some(setter) = handler.setter {
      let state = unsafe {
        call_named_setter(ctx, receiver, prop, value, handler, setter)
      };
      if state != 0 {
        return state;
      }
    }
  }
  unsafe {
    JS_DefinePropertyValue(
      ctx,
      receiver,
      prop,
      JS_DupValue(ctx, value),
      JS_PROP_C_W_E,
    )
  }
}

unsafe extern "C" fn named_handler_delete_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
) -> c_int {
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return 1;
  };

  if let (Some(index), Some(handler)) = (
    unsafe { atom_to_u32_index(ctx, prop) },
    inst.indexed_handler.as_ref(),
  ) {
    if let Some(deleter) = handler.deleter {
      let (state, ok) =
        unsafe { call_indexed_deleter(ctx, obj, index, handler, deleter) };
      if state < 0 {
        return -1;
      }
      if state > 0 {
        return ok;
      }
    }
  }

  if let Some(handler) = inst.named_handler.as_ref() {
    if let Some(deleter) = handler.deleter {
      let state =
        unsafe { call_named_deleter(ctx, obj, prop, handler, deleter) };
      if state < 0 {
        return -1;
      }
      if state > 0 {
        return 1;
      }
    }
  }

  1
}

unsafe extern "C" fn named_handler_get_own_property_names(
  ctx: *mut JSContext,
  ptab: *mut *mut JSPropertyEnum,
  plen: *mut u32,
  obj: JSValue,
) -> c_int {
  if ptab.is_null() || plen.is_null() {
    return 0;
  }
  unsafe {
    *ptab = ptr::null_mut();
    *plen = 0;
  }
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return 0;
  };
  let arr = if let Some(handler) = inst.indexed_handler.as_ref() {
    if let Some(enumerator) = handler.enumerator {
      unsafe { call_indexed_enumerator(ctx, obj, handler, enumerator) }
    } else if let Some(handler) = inst.named_handler.as_ref() {
      if let Some(enumerator) = handler.enumerator {
        unsafe { call_named_enumerator(ctx, obj, handler, enumerator) }
      } else {
        return 0;
      }
    } else {
      return 0;
    }
  } else if let Some(handler) = inst.named_handler.as_ref() {
    if let Some(enumerator) = handler.enumerator {
      unsafe { call_named_enumerator(ctx, obj, handler, enumerator) }
    } else {
      return 0;
    }
  } else {
    return 0;
  };
  if jsv_is_exception(&arr) {
    return -1;
  }
  if jsv_is_undefined(&arr) {
    return 0;
  }

  let mut len: i64 = 0;
  if unsafe { JS_GetLength(ctx, arr, &mut len) } < 0 || len <= 0 {
    unsafe { JS_FreeValue(ctx, arr) };
    return 0;
  }
  let count = len.min(u32::MAX as i64) as u32;
  let bytes = count as usize * std::mem::size_of::<JSPropertyEnum>();
  let tab = unsafe { js_malloc(ctx, bytes) as *mut JSPropertyEnum };
  if tab.is_null() {
    unsafe { JS_FreeValue(ctx, arr) };
    return -1;
  }
  let mut written = 0_u32;
  for i in 0..count {
    let item = unsafe { JS_GetPropertyUint32(ctx, arr, i) };
    if jsv_is_exception(&item) {
      unsafe {
        JS_FreeValue(ctx, arr);
        js_free(ctx, tab as *mut c_void);
      }
      return -1;
    }
    let atom = unsafe { JS_ValueToAtom(ctx, item) };
    unsafe { JS_FreeValue(ctx, item) };
    if atom == 0 {
      continue;
    }
    unsafe {
      (*tab.add(written as usize)).is_enumerable = true;
      (*tab.add(written as usize)).atom = atom;
    }
    written += 1;
  }
  unsafe {
    JS_FreeValue(ctx, arr);
    *ptab = tab;
    *plen = written;
  }
  0
}

unsafe extern "C" fn named_handler_define_own_property(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: JSAtom,
  val: JSValue,
  getter: JSValue,
  setter: JSValue,
  flags: c_int,
) -> c_int {
  let Some(inst) = (unsafe { named_handler_from_obj(obj) }) else {
    return 0;
  };

  if let (Some(index), Some(handler)) = (
    unsafe { atom_to_u32_index(ctx, prop) },
    inst.indexed_handler.as_ref(),
  ) {
    if let Some(definer) = handler.definer {
      let state = unsafe {
        call_indexed_definer(
          ctx, obj, index, val, getter, setter, flags, handler, definer,
        )
      };
      if state != 0 {
        return state;
      }
    }
  }

  if let Some(handler) = inst.named_handler.as_ref() {
    if let Some(definer) = handler.definer {
      let state = unsafe {
        call_named_definer(
          ctx, obj, prop, val, getter, setter, flags, handler, definer,
        )
      };
      if state != 0 {
        return state;
      }
    }
  }
  unsafe {
    JS_DefineProperty(
      ctx,
      obj,
      prop,
      JS_DupValue(ctx, val),
      JS_DupValue(ctx, getter),
      JS_DupValue(ctx, setter),
      flags | JS_PROP_NO_EXOTIC_QJS,
    )
  }
}

static NAMED_HANDLER_EXOTIC: JSClassExoticMethods = JSClassExoticMethods {
  get_own_property: Some(named_handler_get_own_property),
  get_own_property_names: Some(named_handler_get_own_property_names),
  delete_property: Some(named_handler_delete_property),
  define_own_property: Some(named_handler_define_own_property),
  has_property: None,
  get_property: Some(named_handler_get_property),
  set_property: Some(named_handler_set_property),
};

unsafe fn throw_termination_exception(ctx: *mut JSContext) -> JSValue {
  unsafe {
    JS_ThrowInternalError(ctx, c"interrupted".as_ptr());
    let exc = JS_GetException(ctx);
    JS_SetUncatchableError(ctx, exc);
    JS_Throw(ctx, exc)
  }
}

unsafe fn dispatch(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  this: JSValue,
  new_target: JSValue,
  is_construct: bool,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let iso = current_iso();

  // Emulate `v8::Isolate::TerminateExecution`: once a termination is pending, V8
  // throws an uncatchable exception at the next safe point and runs no further
  // JS until it is cancelled. The most important safe point for deno_core is the
  // op call itself — e.g. after an unhandled-rejection handler calls
  // `reportUnhandledException` (which dispatches an exception and terminates),
  // the runtime would otherwise go on to `op_dispatch_exception` the *original*
  // rejection, overwriting the reported one. Refuse to run the op and surface an
  // uncatchable "interrupted" error instead.
  if !iso.is_null() && iso_state(iso).is_terminating() {
    return unsafe { throw_termination_exception(ctx) };
  }

  let n = argc.max(0) as usize;
  let mut args = Vec::with_capacity(n);
  for i in 0..n {
    args.push(unsafe { *argv.add(i) });
  }

  // Every handle the callback interns (its arguments, `data`, `this`, any Local
  // it creates) lands in the isolate's handle arena. V8 frees those when the
  // callback's implicit HandleScope unwinds; we have no such scope, so without
  // this boundary they accumulate for the lifetime of the *enclosing* scope —
  // which, for an op called in a 6000-iteration JIT-warmup loop, means tens of
  // thousands of live arena slots, each pinning a QuickJS reference (notably the
  // op's `data` External, whose refcount then balloons into the thousands).
  // Bracket each callback with a save/restore of the arena depth so it behaves
  // like V8's per-callback HandleScope. Record the depth now and release
  // everything the callback added once we're done.
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this,
    data,
    new_target,
    is_construct,
    args,
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info) as *const FunctionCallbackInfo;
  unsafe { (callback)(info_ptr) };

  let info = unsafe { Box::from_raw(info_ptr as *mut CbInfo) };
  let ret = *info.return_slot;

  // Materialize the result as a value we own *before* tearing down the arena:
  // `ret` is only a borrowed copy of whatever Local the callback stored via
  // ReturnValue::set, and that Local lives in the slots we're about to free.
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if jsv_is_undefined(&ret) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, ret) }
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }

  result
}

unsafe fn try_fast_dispatch(
  ctx: *mut JSContext,
  fast_overloads: &[RawCFunction],
  data: JSValue,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> Option<JSValue> {
  if fast_overloads.is_empty() || unsafe { fast_api_next_call_count(ctx) } <= 0
  {
    return None;
  }

  for overload in fast_overloads {
    if let Some(result) =
      unsafe { call_fast_overload(ctx, overload, data, this_val, argc, argv) }
    {
      unsafe { consume_fast_api_next_call(ctx) };
      return Some(result);
    }
  }
  None
}

unsafe fn fast_api_next_call_count(ctx: *mut JSContext) -> i32 {
  if ctx.is_null() {
    return 0;
  }
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let value = unsafe {
    JS_GetPropertyStr(ctx, global, c"__v8x_fast_api_next_call".as_ptr())
  };
  unsafe { JS_FreeValue(ctx, global) };
  if value.tag == JS_TAG_EXCEPTION {
    unsafe {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    return 0;
  }
  let mut count = 0;
  if unsafe { JS_ToInt32(ctx, &mut count, value) } < 0 {
    unsafe {
      JS_FreeValue(ctx, value);
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    return 0;
  }
  unsafe { JS_FreeValue(ctx, value) };
  count
}

unsafe fn consume_fast_api_next_call(ctx: *mut JSContext) {
  let count = unsafe { fast_api_next_call_count(ctx) };
  if count <= 0 || ctx.is_null() {
    return;
  }
  let global = unsafe { JS_GetGlobalObject(ctx) };
  unsafe {
    JS_SetPropertyStr(
      ctx,
      global,
      c"__v8x_fast_api_next_call".as_ptr(),
      JS_NewInt32(ctx, count - 1),
    );
    JS_FreeValue(ctx, global);
  }
}

unsafe fn call_fast_overload(
  ctx: *mut JSContext,
  overload: &RawCFunction,
  data: JSValue,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> Option<JSValue> {
  if overload.address_.is_null() || overload.type_info_.is_null() {
    return None;
  }
  let info = unsafe { &*overload.type_info_ };
  let arg_count = info.arg_count_ as usize;
  if arg_count == 0 || info.arg_info_.is_null() {
    return None;
  }
  let has_options = unsafe {
    (*info.arg_info_.add(arg_count - 1)).type_ == CTYPE_CALLBACK_OPTIONS
  };
  let effective_arg_count = if has_options {
    arg_count - 1
  } else {
    arg_count
  };
  if effective_arg_count != argc.max(0) as usize + 1 {
    return None;
  }

  let mut arg_types = Vec::with_capacity(effective_arg_count);
  for i in 0..effective_arg_count {
    arg_types.push(unsafe { (*info.arg_info_.add(i)).type_ });
  }
  if arg_types.first().copied() != Some(CTYPE_V8_VALUE) {
    return None;
  }

  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };

  let recv = unsafe { fast_local_object(ctx, this_val) };
  let mut options = unsafe { fast_options(ctx, iso, data) };
  let options_ptr =
    &mut options as *mut crate::fast_api::FastApiCallbackOptions<'static>;

  let result =
    match (info.return_info_.type_, arg_types.as_slice(), has_options) {
      (CTYPE_POINTER, [CTYPE_V8_VALUE], true) => {
        let f: unsafe fn(
          Local<'static, Object>,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> *mut c_void = unsafe { std::mem::transmute(overload.address_) };
        let out = unsafe { f(recv, options_ptr) };
        if out.is_null() {
          jsv_null()
        } else {
          make_external_jsvalue(iso, ctx, out)
        }
      }
      (CTYPE_BOOL, [CTYPE_V8_VALUE, CTYPE_V8_VALUE], true) => {
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 0)?) };
        let f: unsafe fn(
          Local<'static, Object>,
          Local<'static, Value>,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> bool = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewBool(ctx, f(recv, value, options_ptr) as c_int) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_UINT32, CTYPE_UINT32], true) => {
        let a = unsafe { fast_u32(ctx, js_argv(argc, argv, 0)?)? };
        let b = unsafe { fast_u32(ctx, js_argv(argc, argv, 1)?)? };
        let f: unsafe fn(
          Local<'static, Object>,
          u32,
          u32,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, a, b, options_ptr)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_UINT64, CTYPE_UINT64], true) => {
        let a = unsafe { fast_u64(ctx, js_argv(argc, argv, 0)?, info.repr_)? };
        let b = unsafe { fast_u64(ctx, js_argv(argc, argv, 1)?, info.repr_)? };
        let f: unsafe fn(
          Local<'static, Object>,
          u64,
          u64,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, a, b, options_ptr)) }
      }
      (CTYPE_FLOAT32, [CTYPE_V8_VALUE, CTYPE_FLOAT32, CTYPE_FLOAT32], true) => {
        let a = unsafe { fast_f64(ctx, js_argv(argc, argv, 0)?)? } as f32;
        let b = unsafe { fast_f64(ctx, js_argv(argc, argv, 1)?)? } as f32;
        let f: unsafe fn(
          Local<'static, Object>,
          f32,
          f32,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> f32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewFloat64(ctx, f(recv, a, b, options_ptr) as f64) }
      }
      (CTYPE_FLOAT64, [CTYPE_V8_VALUE, CTYPE_FLOAT64, CTYPE_FLOAT64], true) => {
        let a = unsafe { fast_f64(ctx, js_argv(argc, argv, 0)?)? };
        let b = unsafe { fast_f64(ctx, js_argv(argc, argv, 1)?)? };
        let f: unsafe fn(
          Local<'static, Object>,
          f64,
          f64,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> f64 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewFloat64(ctx, f(recv, a, b, options_ptr)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_UINT32], true) => {
        let value = unsafe { fast_u32(ctx, js_argv(argc, argv, 0)?)? };
        let f: unsafe fn(
          Local<'static, Object>,
          u32,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, value, options_ptr)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_V8_VALUE, CTYPE_UINT64], true) => {
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 0)?) };
        let len =
          unsafe { fast_u64(ctx, js_argv(argc, argv, 1)?, info.repr_)? };
        let f: unsafe fn(
          Local<'static, Object>,
          Local<'static, Value>,
          u64,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, value, len, options_ptr)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_V8_VALUE, CTYPE_UINT32], true) => {
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 0)?) };
        let len = unsafe { fast_u32(ctx, js_argv(argc, argv, 1)?)? };
        let f: unsafe fn(
          Local<'static, Object>,
          Local<'static, Value>,
          u32,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, value, len, options_ptr)) }
      }
      (CTYPE_VOID, [CTYPE_V8_VALUE, CTYPE_V8_VALUE], false) => {
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 0)?) };
        let f: unsafe fn(Local<'static, Object>, Local<'static, Value>) =
          unsafe { std::mem::transmute(overload.address_) };
        unsafe { f(recv, value) };
        jsv_undefined()
      }
      (
        CTYPE_UINT32,
        [CTYPE_V8_VALUE, CTYPE_UINT32, CTYPE_UINT32, CTYPE_V8_VALUE],
        false,
      ) => {
        let a = unsafe { fast_u32(ctx, js_argv(argc, argv, 0)?)? };
        let b = unsafe { fast_u32(ctx, js_argv(argc, argv, 1)?)? };
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 2)?) };
        let f: unsafe fn(
          Local<'static, Object>,
          u32,
          u32,
          Local<'static, Value>,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, a, b, value)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_V8_VALUE], false) => {
        let value = unsafe { fast_local_value(ctx, js_argv(argc, argv, 0)?) };
        let f: unsafe fn(Local<'static, Object>, Local<'static, Value>) -> u32 =
          unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv, value)) }
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE], false) => {
        let f: unsafe fn(Local<'static, Object>) -> u32 =
          unsafe { std::mem::transmute(overload.address_) };
        unsafe { JS_NewUint32(ctx, f(recv)) }
      }
      (CTYPE_VOID, [CTYPE_V8_VALUE, CTYPE_UINT32], false) => {
        let a = unsafe { fast_u32(ctx, js_argv(argc, argv, 0)?)? };
        let f: unsafe fn(Local<'static, Object>, u32) =
          unsafe { std::mem::transmute(overload.address_) };
        unsafe { f(recv, a) };
        jsv_undefined()
      }
      (CTYPE_VOID, [CTYPE_V8_VALUE, CTYPE_UINT32, CTYPE_UINT32], false) => {
        let a = unsafe { fast_u32(ctx, js_argv(argc, argv, 0)?)? };
        let b = unsafe { fast_u32(ctx, js_argv(argc, argv, 1)?)? };
        let f: unsafe fn(Local<'static, Object>, u32, u32) =
          unsafe { std::mem::transmute(overload.address_) };
        unsafe { f(recv, a, b) };
        jsv_undefined()
      }
      (CTYPE_VOID, [CTYPE_V8_VALUE], true) => {
        let f: unsafe fn(
          Local<'static, Object>,
          *mut crate::fast_api::FastApiCallbackOptions<'static>,
        ) = unsafe { std::mem::transmute(overload.address_) };
        unsafe { f(recv, options_ptr) };
        jsv_undefined()
      }
      (CTYPE_UINT32, [CTYPE_V8_VALUE, CTYPE_SEQ_ONE_BYTE_STRING], false) => {
        let (string, cstr) =
          unsafe { fast_one_byte_string(ctx, js_argv(argc, argv, 0)?)? };
        let f: unsafe fn(
          Local<'static, Object>,
          *const crate::fast_api::FastApiOneByteString,
        ) -> u32 = unsafe { std::mem::transmute(overload.address_) };
        let out = unsafe { f(recv, &*string) };
        unsafe { JS_FreeCString(ctx, cstr) };
        unsafe { JS_NewUint32(ctx, out) }
      }
      (CTYPE_UINT64, [CTYPE_V8_VALUE, CTYPE_UINT64, CTYPE_UINT64], false) => {
        let a = unsafe { fast_u64(ctx, js_argv(argc, argv, 0)?, info.repr_)? };
        let b = unsafe { fast_u64(ctx, js_argv(argc, argv, 1)?, info.repr_)? };
        let f: unsafe fn(Local<'static, Object>, u64, u64) -> u64 =
          unsafe { std::mem::transmute(overload.address_) };
        let out = unsafe { f(recv, a, b) };
        if info.repr_ == INT64_REPR_BIGINT {
          unsafe { JS_NewBigUint64(ctx, out) }
        } else {
          unsafe { JS_NewFloat64(ctx, out as f64) }
        }
      }
      (CTYPE_POINTER, [CTYPE_V8_VALUE, CTYPE_POINTER], false) => {
        let ptr = unsafe { fast_pointer_arg(js_argv(argc, argv, 0)?)? };
        let f: unsafe fn(Local<'static, Object>, *mut c_void) -> *mut c_void =
          unsafe { std::mem::transmute(overload.address_) };
        let out = unsafe { f(recv, ptr) };
        if out.is_null() {
          jsv_null()
        } else {
          make_external_jsvalue(iso, ctx, out)
        }
      }
      _ => {
        unsafe { restore_handle_depth(iso, saved_depth) };
        return None;
      }
    };

  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else {
    result
  };
  unsafe { restore_handle_depth(iso, saved_depth) };
  Some(result)
}

unsafe fn restore_handle_depth(iso: *mut RealIsolate, saved_depth: usize) {
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  while st.handles.len() > saved_depth {
    if let Some(slot) = st.handles.pop() {
      unsafe {
        JS_FreeValue(st.ctx, *slot);
        drop(Box::from_raw(slot));
      }
    }
  }
}

unsafe fn js_argv(
  argc: c_int,
  argv: *mut JSValue,
  index: usize,
) -> Option<JSValue> {
  if argv.is_null() || index >= argc.max(0) as usize {
    return None;
  }
  Some(unsafe { *argv.add(index) })
}

unsafe fn fast_local_value(
  ctx: *mut JSContext,
  value: JSValue,
) -> Local<'static, Value> {
  let handle = intern_dup::<Value>(ctx, value);
  unsafe { Local::from_raw_unchecked(handle) }
}

unsafe fn fast_local_object(
  ctx: *mut JSContext,
  value: JSValue,
) -> Local<'static, Object> {
  let handle = intern_dup::<Object>(ctx, value);
  unsafe { Local::from_raw_unchecked(handle) }
}

unsafe fn fast_options(
  ctx: *mut JSContext,
  iso: *mut RealIsolate,
  data: JSValue,
) -> crate::fast_api::FastApiCallbackOptions<'static> {
  crate::fast_api::FastApiCallbackOptions {
    isolate: iso,
    data: unsafe { fast_local_value(ctx, data) },
  }
}

unsafe fn fast_u32(ctx: *mut JSContext, value: JSValue) -> Option<u32> {
  let mut out = 0i32;
  if unsafe { JS_ToInt32(ctx, &mut out, value) } < 0 {
    unsafe {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    None
  } else {
    Some(out as u32)
  }
}

unsafe fn fast_f64(ctx: *mut JSContext, value: JSValue) -> Option<f64> {
  let mut out = 0f64;
  if unsafe { JS_ToFloat64(ctx, &mut out, value) } < 0 {
    unsafe {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    None
  } else {
    Some(out)
  }
}

unsafe extern "C" {
  fn JS_ToBigUint64(ctx: *mut JSContext, pres: *mut u64, val: JSValue)
  -> c_int;
}

unsafe fn fast_u64(
  ctx: *mut JSContext,
  value: JSValue,
  repr: u8,
) -> Option<u64> {
  if repr == INT64_REPR_BIGINT {
    let mut out = 0u64;
    if unsafe { JS_ToBigUint64(ctx, &mut out, value) } < 0 {
      unsafe {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
      }
      return None;
    }
    Some(out)
  } else {
    let mut out = 0f64;
    if unsafe { JS_ToFloat64(ctx, &mut out, value) } < 0 {
      unsafe {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
      }
      return None;
    }
    Some(out as u64)
  }
}

unsafe fn fast_pointer_arg(value: JSValue) -> Option<*mut c_void> {
  if jsv_is_null(&value) || jsv_is_undefined(&value) {
    return Some(ptr::null_mut());
  }
  external_pointer_from_value(value)
}

unsafe fn fast_one_byte_string(
  ctx: *mut JSContext,
  value: JSValue,
) -> Option<(Box<crate::fast_api::FastApiOneByteString>, *const c_char)> {
  let mut len = 0usize;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, value) };
  if cstr.is_null() {
    unsafe {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    return None;
  }
  Some((
    Box::new(crate::fast_api::FastApiOneByteString {
      data: cstr,
      length: len as u32,
    }),
    cstr,
  ))
}

pub(crate) unsafe fn call_callback_for_wasm(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  args: &mut [JSValue],
) -> JSValue {
  unsafe {
    dispatch(
      ctx,
      callback,
      data,
      jsv_undefined(),
      jsv_undefined(),
      false,
      args.len() as c_int,
      args.as_mut_ptr(),
    )
  }
}

fn receiver_matches_signature(
  ctx: *mut JSContext,
  recv: JSValue,
  signature: *const FnTemplate,
) -> bool {
  if signature.is_null() {
    return true;
  }
  if ctx.is_null() || !jsv_is_object(&recv) {
    return false;
  }

  let expected = unsafe { build_prototype_object(ctx, signature) };
  if !jsv_is_object(&expected) {
    return false;
  }

  let mut current = unsafe { JS_DupValue(ctx, recv) };
  for _ in 0..64 {
    if unsafe { JS_IsStrictEqual(ctx, current, expected) } {
      unsafe { JS_FreeValue(ctx, current) };
      return true;
    }

    let next = unsafe { JS_GetPrototype(ctx, current) };
    unsafe { JS_FreeValue(ctx, current) };
    if next.tag == JS_TAG_EXCEPTION {
      return false;
    }
    if !jsv_is_object(&next) {
      unsafe { JS_FreeValue(ctx, next) };
      return false;
    }
    current = next;
  }

  unsafe { JS_FreeValue(ctx, current) };
  false
}

unsafe extern "C" fn fn_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some((callback, data, _instance, signature, fast_overloads)) =
    lookup_dispatch(ctx, magic)
  else {
    return jsv_undefined();
  };
  if !receiver_matches_signature(ctx, this_val, signature) {
    unsafe { JS_FreeValue(ctx, data) };
    return unsafe { JS_ThrowTypeError(ctx, c"Illegal invocation".as_ptr()) };
  }
  if let Some(result) = unsafe {
    try_fast_dispatch(ctx, &fast_overloads, data, this_val, argc, argv)
  } {
    unsafe { JS_FreeValue(ctx, data) };
    return result;
  }
  let r = unsafe {
    dispatch(
      ctx,
      callback,
      data,
      this_val,
      jsv_undefined(),
      false,
      argc,
      argv,
    )
  };
  unsafe { JS_FreeValue(ctx, data) };
  r
}

unsafe extern "C" fn fn_construct_trampoline(
  ctx: *mut JSContext,
  this_or_new_target: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some((callback, data, instance, _signature, _fast_overloads)) =
    lookup_dispatch(ctx, magic)
  else {
    return unsafe { JS_NewObject(ctx) };
  };

  if !unsafe { v82jsc_is_constructor_call(ctx) } {
    let r = unsafe {
      dispatch(
        ctx,
        callback,
        data,
        this_or_new_target,
        jsv_undefined(),
        false,
        argc,
        argv,
      )
    };
    unsafe { JS_FreeValue(ctx, data) };
    return r;
  }
  let new_target = this_or_new_target;

  let this = if !instance.is_null() {
    let t = unsafe { (&*instance).state() };
    new_object_for_template(ctx, t)
  } else {
    unsafe { JS_NewObject(ctx) }
  };
  if jsv_is_exception(&this) {
    unsafe { JS_FreeValue(ctx, data) };
    return this;
  }
  unsafe {
    let proto = JS_GetPropertyStr(ctx, new_target, c"prototype".as_ptr());
    if std::env::var_os("QJS_DEBUG_TMPL").is_some() {
      let is_obj = jsv_is_object(&proto);
      let has_log = if is_obj {
        let l = JS_GetPropertyStr(ctx, proto, c"log".as_ptr());
        let f = !jsv_is_undefined(&l);
        JS_FreeValue(ctx, l);
        f
      } else {
        false
      };
      eprintln!(
        "[QJS construct] proto_is_obj={is_obj} proto_has_log={has_log}"
      );
    }
    if jsv_is_object(&proto) {
      JS_SetPrototype(ctx, this, proto);
    }
    JS_FreeValue(ctx, proto);
  }

  if !instance.is_null() {
    let t = unsafe { (&*instance).state() };
    super::object::set_internal_field_count_for_value(
      this,
      t.internal_field_count,
    );
    if !t.parent_fn.is_null() {
      super::object::mark_api_wrapper_for_value(this);
    }
    apply_props(ctx, this, &t.props);
    apply_accessors(ctx, this, &t.accessors);
  }

  let r = unsafe {
    dispatch(ctx, callback, data, this, new_target, true, argc, argv)
  };
  unsafe { JS_FreeValue(ctx, data) };
  if jsv_is_exception(&r) {
    unsafe { JS_FreeValue(ctx, this) };
    return r;
  }

  let result = if jsv_is_object(&r) {
    unsafe { JS_FreeValue(ctx, this) };
    r
  } else {
    unsafe { JS_FreeValue(ctx, r) };
    this
  };
  result
}

unsafe extern "C" fn named_getter_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some(entry) = lookup_named_getter(magic) else {
    return jsv_undefined();
  };

  if !entry.owner_ctx.is_null()
    && entry.owner_ctx != ctx
    && !super::misc::contexts_share_security_token(ctx, entry.owner_ctx)
  {
    return unsafe { JS_ThrowTypeError(ctx, c"no access".as_ptr()) };
  }

  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let (callback_data, free_callback_data) =
    unsafe { materialize_named_handler_data(ctx, entry) };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: callback_data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Value>;

  let key = unsafe { JS_AtomToValue(ctx, entry.atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    return jsv_undefined();
  };
  let intercepted =
    unsafe { (entry.getter)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let intercepted = unsafe { std::mem::transmute_copy::<_, u32>(&intercepted) };
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if intercepted == 0 {
    if jsv_is_undefined(&ret) {
      jsv_undefined()
    } else {
      unsafe { JS_DupValue(ctx, ret) }
    }
  } else {
    jsv_undefined()
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }
  if free_callback_data {
    unsafe { JS_FreeValue(ctx, callback_data) };
  }

  result
}

struct ParsedDefineDescriptor {
  value: JSValue,
  getter: JSValue,
  setter: JSValue,
  flags: c_int,
}

unsafe fn free_parsed_define_descriptor(
  ctx: *mut JSContext,
  desc: ParsedDefineDescriptor,
) {
  unsafe {
    JS_FreeValue(ctx, desc.value);
    JS_FreeValue(ctx, desc.getter);
    JS_FreeValue(ctx, desc.setter);
  }
}

unsafe fn parse_descriptor_bool(
  ctx: *mut JSContext,
  obj: JSValue,
  name: *const c_char,
  has_flag: c_int,
  value_flag: c_int,
  flags: &mut c_int,
) -> c_int {
  let has = unsafe { JS_HasPropertyStr(ctx, obj, name) };
  if has <= 0 {
    return has;
  }
  let value = unsafe { JS_GetPropertyStr(ctx, obj, name) };
  if jsv_is_exception(&value) {
    return -1;
  }
  *flags |= has_flag;
  if unsafe { JS_ToBool(ctx, value) } != 0 {
    *flags |= value_flag;
  }
  unsafe { JS_FreeValue(ctx, value) };
  0
}

unsafe fn parse_define_descriptor(
  ctx: *mut JSContext,
  desc_obj: JSValue,
) -> Option<ParsedDefineDescriptor> {
  let mut desc = ParsedDefineDescriptor {
    value: jsv_undefined(),
    getter: jsv_undefined(),
    setter: jsv_undefined(),
    flags: 0,
  };

  let has_value =
    unsafe { JS_HasPropertyStr(ctx, desc_obj, c"value".as_ptr()) };
  if has_value < 0 {
    return None;
  }
  if has_value > 0 {
    let value = unsafe { JS_GetPropertyStr(ctx, desc_obj, c"value".as_ptr()) };
    if jsv_is_exception(&value) {
      return None;
    }
    desc.value = value;
    desc.flags |= JS_PROP_HAS_VALUE_QJS;
  }

  let has_get = unsafe { JS_HasPropertyStr(ctx, desc_obj, c"get".as_ptr()) };
  if has_get < 0 {
    unsafe { free_parsed_define_descriptor(ctx, desc) };
    return None;
  }
  if has_get > 0 {
    let getter = unsafe { JS_GetPropertyStr(ctx, desc_obj, c"get".as_ptr()) };
    if jsv_is_exception(&getter) {
      unsafe { free_parsed_define_descriptor(ctx, desc) };
      return None;
    }
    desc.getter = getter;
    desc.flags |= JS_PROP_HAS_GET_QJS;
  }

  let has_set = unsafe { JS_HasPropertyStr(ctx, desc_obj, c"set".as_ptr()) };
  if has_set < 0 {
    unsafe { free_parsed_define_descriptor(ctx, desc) };
    return None;
  }
  if has_set > 0 {
    let setter = unsafe { JS_GetPropertyStr(ctx, desc_obj, c"set".as_ptr()) };
    if jsv_is_exception(&setter) {
      unsafe { free_parsed_define_descriptor(ctx, desc) };
      return None;
    }
    desc.setter = setter;
    desc.flags |= JS_PROP_HAS_SET_QJS;
  }

  if unsafe {
    parse_descriptor_bool(
      ctx,
      desc_obj,
      c"enumerable".as_ptr(),
      JS_PROP_HAS_ENUMERABLE_QJS,
      JS_PROP_ENUMERABLE,
      &mut desc.flags,
    )
  } < 0
  {
    unsafe { free_parsed_define_descriptor(ctx, desc) };
    return None;
  }
  if unsafe {
    parse_descriptor_bool(
      ctx,
      desc_obj,
      c"configurable".as_ptr(),
      JS_PROP_HAS_CONFIGURABLE_QJS,
      JS_PROP_CONFIGURABLE,
      &mut desc.flags,
    )
  } < 0
  {
    unsafe { free_parsed_define_descriptor(ctx, desc) };
    return None;
  }
  if unsafe {
    parse_descriptor_bool(
      ctx,
      desc_obj,
      c"writable".as_ptr(),
      JS_PROP_HAS_WRITABLE_QJS,
      JS_PROP_WRITABLE,
      &mut desc.flags,
    )
  } < 0
  {
    unsafe { free_parsed_define_descriptor(ctx, desc) };
    return None;
  }

  Some(desc)
}

unsafe extern "C" fn global_define_property_trampoline(
  ctx: *mut JSContext,
  _this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some(entry) = lookup_global_define(magic) else {
    return jsv_exception();
  };

  if argc >= 3 && !argv.is_null() {
    let target = unsafe { *argv };
    let desc_obj = unsafe { *argv.add(2) };
    if unsafe { JS_IsStrictEqual(ctx, target, entry.global) }
      && jsv_is_object(&desc_obj)
    {
      let atom = unsafe { JS_ValueToAtom(ctx, *argv.add(1)) };
      if atom == 0 && unsafe { JS_HasException(ctx) } {
        return jsv_exception();
      }

      let Some(desc) = (unsafe { parse_define_descriptor(ctx, desc_obj) })
      else {
        unsafe { JS_FreeAtom(ctx, atom) };
        return jsv_exception();
      };

      let mut intercepted = false;
      if let Some(descriptor) = entry.handler.descriptor {
        let (state, value) = unsafe {
          call_named_getter(ctx, target, atom, &entry.handler, descriptor)
        };
        if state < 0 {
          unsafe {
            JS_FreeAtom(ctx, atom);
            free_parsed_define_descriptor(ctx, desc);
          }
          return jsv_exception();
        }
        if state > 0 {
          unsafe { JS_FreeValue(ctx, value) };
        }
      }

      if desc.flags & JS_PROP_HAS_VALUE_QJS != 0 {
        if let Some(setter) = entry.handler.setter {
          let state = unsafe {
            call_named_setter(
              ctx,
              target,
              atom,
              desc.value,
              &entry.handler,
              setter,
            )
          };
          if state < 0 {
            unsafe {
              JS_FreeAtom(ctx, atom);
              free_parsed_define_descriptor(ctx, desc);
            }
            return jsv_exception();
          }
          intercepted |= state > 0;
        }
      }

      if let Some(definer) = entry.handler.definer {
        let state = unsafe {
          call_named_definer(
            ctx,
            target,
            atom,
            desc.value,
            desc.getter,
            desc.setter,
            desc.flags,
            &entry.handler,
            definer,
          )
        };
        if state < 0 {
          unsafe {
            JS_FreeAtom(ctx, atom);
            free_parsed_define_descriptor(ctx, desc);
          }
          return jsv_exception();
        }
        intercepted |= state > 0;
      }

      unsafe {
        JS_FreeAtom(ctx, atom);
        free_parsed_define_descriptor(ctx, desc);
      }

      if intercepted {
        return unsafe { JS_DupValue(ctx, target) };
      }
    }
  }

  unsafe {
    JS_Call(
      ctx,
      entry.original,
      entry.object_ctor,
      argc,
      if argc > 0 { argv } else { ptr::null_mut() },
    )
  }
}

pub(crate) unsafe fn call_accessor_name_getter(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  data: JSValue,
  getter: crate::AccessorNameGetterCallback,
) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }

  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Value>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    return jsv_undefined();
  };
  unsafe { (getter)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if jsv_is_undefined(&ret) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, ret) }
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }

  result
}

pub(crate) unsafe fn call_accessor_name_setter(
  ctx: *mut JSContext,
  this_val: JSValue,
  atom: JSAtom,
  value: JSValue,
  data: JSValue,
  setter: crate::AccessorNameSetterCallback,
) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }

  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info =
    &mut raw_info as *mut *mut c_void as *const crate::PropertyCallbackInfo<()>;

  let key = unsafe { JS_AtomToValue(ctx, atom) };
  let key_handle = intern::<Name>(key);
  let value_handle = intern_dup::<Value>(ctx, value);
  let (Some(key_handle), Some(value_handle)) = (
    NonNull::new(key_handle as *mut Name),
    NonNull::new(value_handle as *mut Value),
  ) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    return jsv_undefined();
  };
  unsafe {
    (setter)(
      crate::SealedLocal(key_handle),
      crate::SealedLocal(value_handle),
      prop_info,
    )
  };

  let _info = unsafe { Box::from_raw(info_ptr) };
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else {
    jsv_undefined()
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }

  result
}

unsafe fn make_cfunc_magic(
  ctx: *mut JSContext,
  trampoline: unsafe extern "C" fn(
    *mut JSContext,
    JSValue,
    c_int,
    *mut JSValue,
    c_int,
  ) -> JSValue,
  name: *const c_char,
  length: c_int,
  cproto: c_int,
  magic: c_int,
) -> JSValue {
  unsafe {
    let f: JSCFunction = std::mem::transmute::<
      unsafe extern "C" fn(
        *mut JSContext,
        JSValue,
        c_int,
        *mut JSValue,
        c_int,
      ) -> JSValue,
      JSCFunction,
    >(trampoline);
    JS_NewCFunction2(ctx, f, name, length, cproto, magic)
  }
}

unsafe extern "C" fn native_function_to_string(
  ctx: *mut JSContext,
  this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let name = unsafe { JS_GetPropertyStr(ctx, this_val, c"name".as_ptr()) };
  let name_text = if jsv_is_string(&name) {
    let cstr = unsafe { JS_ToCString(ctx, name) };
    let text = if cstr.is_null() {
      std::string::String::new()
    } else {
      unsafe { std::ffi::CStr::from_ptr(cstr) }
        .to_string_lossy()
        .into_owned()
    };
    if !cstr.is_null() {
      unsafe { JS_FreeCString(ctx, cstr) };
    }
    text
  } else {
    std::string::String::new()
  };
  unsafe { JS_FreeValue(ctx, name) };

  let text = if name_text.is_empty() {
    "function () { [native code] }".to_owned()
  } else {
    format!("function {name_text}() {{ [native code] }}")
  };
  unsafe { JS_NewStringLen(ctx, text.as_ptr() as *const c_char, text.len()) }
}

unsafe fn install_native_function_to_string(
  ctx: *mut JSContext,
  function: JSValue,
) {
  let to_string = unsafe {
    JS_NewCFunction(ctx, native_function_to_string, c"toString".as_ptr(), 0)
  };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      function,
      c"toString".as_ptr(),
      to_string,
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
  }
}

pub(crate) unsafe fn make_function_len(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  length: i32,
  construct: bool,
) -> JSValue {
  unsafe {
    make_function_len_with_instance(
      ctx,
      callback,
      data,
      length,
      construct,
      ptr::null(),
      ptr::null(),
      Vec::new(),
    )
  }
}

unsafe fn make_function_len_with_instance(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  length: i32,
  construct: bool,
  instance: *const ObjTemplate,
  signature: *const FnTemplate,
  fast_overloads: Vec<RawCFunction>,
) -> JSValue {
  let data_owned = if jsv_is_undefined(&data) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, data) }
  };
  let data_external = external_pointer_from_value(data);
  let magic = register_dispatch(
    ctx,
    callback,
    data_owned,
    data_external,
    jsv_is_undefined(&data),
    instance,
    signature,
    fast_overloads,
  );
  let cproto = if construct {
    JS_CFUNC_CONSTRUCTOR_OR_FUNC_MAGIC_RECEIVER
  } else {
    JS_CFUNC_GENERIC_MAGIC
  };
  let tramp = if construct {
    fn_construct_trampoline
  } else {
    fn_trampoline
  };
  let f = unsafe {
    make_cfunc_magic(ctx, tramp, ptr::null(), length.max(0), cproto, magic)
  };
  record_snapshot_function(f, callback, data, length, construct);
  unsafe { install_native_function_to_string(ctx, f) };
  f
}

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
  unsafe { &mut *(this as *mut CbInfo) }
}

unsafe fn prop_cbinfo<'a>(this: *const c_void) -> Option<&'a mut CbInfo> {
  if this.is_null() {
    return None;
  }
  let info_ptr = unsafe { *(this as *const *mut c_void) as *mut CbInfo };
  if info_ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *info_ptr })
  }
}

fn property_return_scratch() -> usize {
  thread_local! {
      static SCRATCH: std::cell::UnsafeCell<JSValue> =
          const { std::cell::UnsafeCell::new(JSValue {
              u: JSValueUnion { int32: 0 },
              tag: JS_TAG_UNDEFINED,
          }) };
  }
  SCRATCH.with(|s| s.get() as usize)
}

unsafe extern "C" fn ext_finalize(_rt: *mut JSRuntime, _val: JSValue) {}

unsafe extern "C" fn named_handler_finalize(rt: *mut JSRuntime, val: JSValue) {
  let mut class_id: JSClassID = 0;
  let ptr =
    unsafe { JS_GetAnyOpaque(val, &mut class_id) as *mut NamedHandlerInstance };
  if ptr.is_null() {
    return;
  }
  let inst = unsafe { Box::from_raw(ptr) };
  if let Some(handler) = inst.named_handler {
    if !jsv_is_undefined(&handler.data) {
      unsafe { JS_FreeValueRT(rt, handler.data) };
    }
  }
  if let Some(handler) = inst.indexed_handler {
    if !jsv_is_undefined(&handler.data) {
      unsafe { JS_FreeValueRT(rt, handler.data) };
    }
  }
}

fn external_values()
-> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
  static T: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, usize>>,
  > = std::sync::OnceLock::new();
  T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[derive(Clone)]
struct FunctionScriptInfo {
  line: crate::support::int,
  column: crate::support::int,
  script_id: crate::support::int,
  resource_name: Option<std::string::String>,
  source_map_url: Option<std::string::String>,
}

#[repr(C)]
struct RawScriptOrigin {
  resource_name: usize,
  source_map_url: usize,
  script_id: crate::support::int,
}

thread_local! {
  static FUNCTION_SCRIPT_INFO: std::cell::RefCell<
    std::collections::HashMap<usize, FunctionScriptInfo>,
  > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[inline]
fn external_key(v: JSValue) -> usize {
  jsv_get_ptr(&v) as usize
}

#[inline]
fn function_key(v: JSValue) -> usize {
  jsv_get_ptr(&v) as usize
}

fn external_pointer_from_value(v: JSValue) -> Option<*mut c_void> {
  if !jsv_is_object(&v) {
    return None;
  }
  if let Some(value) = external_values()
    .lock()
    .unwrap()
    .get(&external_key(v))
    .copied()
  {
    return Some(value as *mut c_void);
  }
  let cid = ext_class_id_current();
  if cid == 0 {
    return None;
  }
  let opaque = unsafe { JS_GetOpaque(v, cid) };
  if opaque.is_null() { None } else { Some(opaque) }
}

fn record_snapshot_function(
  function: JSValue,
  callback: FunctionCallback,
  data: JSValue,
  length: i32,
  constructable: bool,
) {
  let key = function_key(function);
  if key == 0 {
    return;
  }
  SNAPSHOT_FUNCTIONS.with(|m| {
    m.borrow_mut().insert(
      key,
      SnapshotFunctionInfo {
        callback,
        data_external: external_pointer_from_value(data),
        length,
        constructable,
      },
    );
  });
}

pub(crate) fn snapshot_function_info(
  function: JSValue,
) -> Option<SnapshotFunctionInfo> {
  let key = function_key(function);
  if key == 0 {
    return None;
  }
  SNAPSHOT_FUNCTIONS.with(|m| m.borrow().get(&key).copied())
}

pub(crate) fn record_function_script_position(
  function: JSValue,
  line: crate::support::int,
  column: crate::support::int,
  script_id: crate::support::int,
  resource_name: Option<std::string::String>,
  source_map_url: Option<std::string::String>,
) {
  let key = function_key(function);
  if key == 0 {
    return;
  }
  FUNCTION_SCRIPT_INFO.with(|m| {
    m.borrow_mut().insert(
      key,
      FunctionScriptInfo {
        line,
        column,
        script_id,
        resource_name,
        source_map_url,
      },
    );
  });
}

fn function_script_info(
  this: *const std::os::raw::c_void,
) -> Option<FunctionScriptInfo> {
  if this.is_null() {
    return None;
  }
  let key = function_key(jsval_of(this as *const Function));
  if key == 0 {
    return None;
  }
  FUNCTION_SCRIPT_INFO.with(|m| m.borrow().get(&key).cloned())
}

fn origin_string_slot(ctx: *mut JSContext, text: &str) -> usize {
  if ctx.is_null() {
    return 0;
  }
  let value =
    unsafe { JS_NewStringLen(ctx, text.as_ptr() as *const c_char, text.len()) };
  if value.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  intern::<Value>(value) as usize
}

/// Allocate a class id and register the `v8::External` class on `rt`. Must be
/// called BEFORE the runtime's first context is created (a context sizes its
/// `class_proto` array to `rt->class_count` at creation time and never grows it,
/// so a class registered afterward makes `JS_NewObjectClass` read `class_proto`
/// out of bounds). Called once per isolate from `v8__Isolate__New`; the id is
/// stored in `IsoState` rather than a thread-local so it stays correct across
/// multiple isolates on one thread (each runtime registers its own class).
pub(crate) fn register_external_class(rt: *mut JSRuntime) -> JSClassID {
  let mut id: JSClassID = 0;
  let id = unsafe { JS_NewClassID(rt, &mut id) };
  let def = JSClassDef {
    class_name: c"v8jsc_external".as_ptr(),
    finalizer: Some(ext_finalize),
    gc_mark: ptr::null(),
    call: ptr::null(),
    exotic: ptr::null(),
  };
  unsafe { JS_NewClass(rt, id, &def) };
  id
}

/// Register the custom class used by `ObjectTemplate::set_named_property_handler`.
/// Like `v8::External`, this must happen before the first context is created so
/// QuickJS sizes every context's class-prototype table correctly.
pub(crate) fn register_named_handler_class(rt: *mut JSRuntime) -> JSClassID {
  let mut id: JSClassID = 0;
  let id = unsafe { JS_NewClassID(rt, &mut id) };
  let def = JSClassDef {
    class_name: c"v8x_named_handler".as_ptr(),
    finalizer: Some(named_handler_finalize),
    gc_mark: ptr::null(),
    call: ptr::null(),
    exotic: &NAMED_HANDLER_EXOTIC,
  };
  unsafe { JS_NewClass(rt, id, &def) };
  id
}

/// The `v8::External` class id for the current isolate (0 if unavailable).
fn ext_class_id_current() -> JSClassID {
  let iso = current_iso();
  if iso.is_null() {
    return 0;
  }
  iso_state(iso).ext_class_id
}

/// Build a v8::External-style wrapper object (owned +1 JSValue). Shared by
/// `v8__External__New` and the snapshot tape replayer.
pub(crate) fn make_external_jsvalue(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  value: *mut c_void,
) -> JSValue {
  if isolate.is_null() || ctx.is_null() {
    return jsv_undefined();
  }
  let cid = iso_state(isolate).ext_class_id;
  let obj = unsafe { JS_NewObjectClass(ctx, cid as c_int) };
  if jsv_is_exception(&obj) {
    return jsv_undefined();
  }
  unsafe { JS_SetOpaque(obj, value) };
  external_values()
    .lock()
    .unwrap()
    .insert(external_key(obj), value as usize);
  obj
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
  isolate: *mut RealIsolate,
  value: *mut c_void,
) -> *const External {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let obj = make_external_jsvalue(isolate, ctx, value);
  if jsv_is_undefined(&obj) {
    return ptr::null();
  }
  let h = intern::<External>(obj);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  let cid = ext_class_id_current();
  if cid == 0 {
    return external_values()
      .lock()
      .unwrap()
      .get(&external_key(jsval_of(this)))
      .copied()
      .unwrap_or(0) as *mut c_void;
  }
  let v = jsval_of(this);
  let opaque = unsafe { JS_GetOpaque(v, cid) };
  if !opaque.is_null() {
    return opaque;
  }
  external_values()
    .lock()
    .unwrap()
    .get(&external_key(v))
    .copied()
    .unwrap_or(0) as *mut c_void
}

pub(crate) fn value_is_external(v: JSValue) -> bool {
  if !jsv_is_object(&v) {
    return false;
  }
  if external_values()
    .lock()
    .unwrap()
    .contains_key(&external_key(v))
  {
    return true;
  }
  let cid = ext_class_id_current();
  if cid == 0 {
    return false;
  }
  !unsafe { JS_GetOpaque(v, cid) }.is_null()
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
  let _ = side_effect_type;
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let construct =
    matches!(constructor_behavior, crate::ConstructorBehavior::Allow);
  let data = jsval_of(data_or_null);
  let f = unsafe { make_function_len(ctx, callback, data, length, construct) };
  let h = intern::<Function>(f);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
  this: *const Function,
  context: *const Context,
  recv: *const Value,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let iso = current_iso();
  if !iso.is_null() && iso_state(iso).is_terminating() {
    unsafe { throw_termination_exception(ctx) };
    return ptr::null();
  }
  let func = jsval_of(this);
  let recv_v = if recv.is_null() {
    jsv_undefined()
  } else {
    jsval_of(recv)
  };
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValue> = Vec::with_capacity(n);
  for i in 0..n {
    args.push(jsval_of(unsafe { *argv.add(i) }));
  }
  let saved_pending = unsafe {
    if JS_HasException(ctx) {
      Some(JS_GetException(ctx))
    } else {
      None
    }
  };
  let r = unsafe { JS_Call(ctx, func, recv_v, n as c_int, args.as_mut_ptr()) };
  if jsv_is_exception(&r) {
    if let Some(saved) = saved_pending {
      unsafe { JS_FreeValue(ctx, saved) };
    }
    if std::env::var("V82JSC_DEBUG").is_ok() {
      unsafe {
        let exc = JS_GetException(ctx);
        let cs = JS_ToCString(ctx, exc);
        if !cs.is_null() {
          eprintln!(
            "[qjs] Function__Call threw: {}",
            std::ffi::CStr::from_ptr(cs).to_string_lossy()
          );
          JS_FreeCString(ctx, cs);
        }

        JS_Throw(ctx, exc);
      }
    }

    return ptr::null();
  }

  let h = intern::<Value>(r);
  if let Some(saved) = saved_pending {
    unsafe {
      JS_Throw(ctx, saved);
    }
  }
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance(
  this: *const Function,
  context: *const Context,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Object {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let func = jsval_of(this);
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValue> = Vec::with_capacity(n);
  for i in 0..n {
    args.push(jsval_of(unsafe { *argv.add(i) }));
  }
  let r =
    unsafe { JS_CallConstructor(ctx, func, n as c_int, args.as_mut_ptr()) };
  if jsv_is_exception(&r) {
    return ptr::null();
  }
  intern::<Object>(r)
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

  const JS_PROP_CONFIGURABLE: c_int = 1 << 0;

  let v = unsafe { JS_DupValue(ctx, jsval_of(name)) };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      jsval_of(this),
      c"name".as_ptr(),
      v,
      JS_PROP_CONFIGURABLE,
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache(
  script: *const Function,
) -> *mut crate::CachedData<'static> {
  let _ = script;
  super::module::make_placeholder_code_cache()
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
  let slot = &mut *info.return_slot as *mut JSValue;

  let data = intern_dup::<Value>(info.ctx, info.data);
  RawFunctionCallbackInfoParts {
    isolate: info.isolate,
    return_value: slot as usize,
    data,
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
  intern_dup::<Value>(info.ctx, info.data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
  this: *const FunctionCallbackInfo,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  intern_dup::<Object>(info.ctx, info.this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__NewTarget(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);

  let nt = if info.is_construct {
    info.new_target
  } else {
    jsv_undefined()
  };
  intern_dup::<Value>(info.ctx, nt)
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
    return intern_dup::<Value>(info.ctx, jsv_undefined());
  }
  match info.args.get(index as usize) {
    Some(&v) => intern_dup::<Value>(info.ctx, v),
    None => intern_dup::<Value>(info.ctx, jsv_undefined()),
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
  (&mut *info.return_slot as *mut JSValue) as usize
}

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut JSValue {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*this).0 as *mut JSValue }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
  this: *mut RawReturnValue,
  value: *const Value,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsval_of(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
  this: *mut RawReturnValue,
  value: bool,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_bool(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
  this: *mut RawReturnValue,
  value: i32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_int32(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
  this: *mut RawReturnValue,
  value: u32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = if value <= i32::MAX as u32 {
      jsv_int32(value as i32)
    } else {
      jsv_float64(value as f64)
    };
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
    unsafe { *slot = jsv_float64(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_null() };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetUndefined(
  this: *mut RawReturnValue,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_undefined() };
  }
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplKind {
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

pub(crate) fn template_kind(p: *const c_void) -> Option<TemplKind> {
  if p.is_null() {
    return None;
  }
  TEMPLATES.with(|t| t.borrow().get(&(p as usize)).copied())
}

pub(crate) fn snapshot_function_template_info(
  template: *const FunctionTemplate,
) -> Option<SnapshotFunctionTemplateInfo> {
  if template_kind(template as *const c_void) != Some(TemplKind::Func) {
    return None;
  }
  let template = unsafe { &*(template as *const FnTemplate) };
  let instance = unsafe { (&*template.instance).state() };
  Some(SnapshotFunctionTemplateInfo {
    callback: template.callback,
    data_external: external_pointer_from_value(template.data),
    length: template.length,
    constructable: template.constructable,
    class_name: template.class_name.clone(),
    cached_proto: jsv_is_object(&template.cached_proto)
      .then_some(template.cached_proto),
    instance_internal_field_count: instance.internal_field_count,
  })
}

pub(crate) fn restore_function_template_from_snapshot(
  callback: FunctionCallback,
  data_external: Option<*mut c_void>,
  length: i32,
  constructable: bool,
  class_name: Option<std::string::String>,
  cached_proto: Option<JSValue>,
  instance_internal_field_count: i32,
) -> *const FunctionTemplate {
  let template =
    tape_make_template(callback, data_external, length, constructable);
  let template_mut = unsafe { &mut *(template as *mut FnTemplate) };
  template_mut.class_name = class_name;
  if let Some(cached_proto) = cached_proto {
    template_mut.cached_proto = cached_proto;
  }
  unsafe {
    (*template_mut.instance).state_mut().internal_field_count =
      instance_internal_field_count;
  }
  template
}

fn with_template_props(
  p: usize,
  f: impl FnOnce(&mut Vec<(JSValue, JSValue, u32)>),
) {
  let kind = TEMPLATES.with(|t| t.borrow().get(&p).copied());
  match kind {
    Some(TemplKind::Func) => {
      let t = unsafe { &mut *(p as *mut FnTemplate) };
      f(&mut t.props);
    }
    Some(TemplKind::Obj) => {
      let t = unsafe { (&*(p as *const ObjTemplate)).state_mut() };
      f(&mut t.props);
    }
    Some(TemplKind::Dict) | None => {}
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

  // The `key` and `value` arrive as borrowed handles (Local) owned by the
  // caller's HandleScope; the props Vec outlives that scope (it lives for the
  // template's lifetime). Storing the raw JSValue would leave a dangling
  // reference once the scope frees the handle — a use-after-free that QuickJS's
  // GC later trips over (NULL/garbage `shape` while marking) once the freed
  // slot is reused. So take our own reference on anything refcounted.
  let ctx = current_ctx();
  let key_owned = own_template_value(ctx, jsval_of(key));
  let stored = if is_template_ptr(value as *const c_void) {
    // A FunctionTemplate/ObjectTemplate pointer tagged for later
    // materialization — a raw pointer, not a refcounted JSValue.
    make_value(
      JS_TAG_TEMPLATE,
      JSValueUnion {
        ptr: value as *mut c_void,
      },
    )
  } else {
    own_template_value(ctx, jsval_of(value))
  };
  with_template_props(raw, |props| {
    props.push((key_owned, stored, attr.as_u32_lenient()));
  });
}

/// Take an owning reference on a JSValue that is about to be stashed in a
/// long-lived template structure (props / accessors). No-op for non-refcounted
/// values (ints, bools, the `JS_TAG_TEMPLATE`/`JS_TAG_V8_CONTEXT` sentinels) and
/// when no context is available.
#[inline]
fn own_template_value(ctx: *mut JSContext, v: JSValue) -> JSValue {
  if ctx.is_null() {
    return v;
  }
  unsafe { JS_DupValue(ctx, v) }
}

const JS_TAG_TEMPLATE: i64 = 0x7633;
const JS_TAG_INTRINSIC: i64 = 0x7634;

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__New(
  isolate: *mut RealIsolate,
  callback: Option<FunctionCallback>,
  data_or_null: *const Value,
  signature_or_null: *const Signature,
  length: i32,
  constructor_behavior: crate::ConstructorBehavior,
  side_effect_type: crate::SideEffectType,
  c_functions: *const crate::fast_api::CFunction,
  c_functions_len: usize,
) -> *const FunctionTemplate {
  let _ = (isolate, side_effect_type);
  // A template without a call handler (`FunctionTemplate::New(isolate)`)
  // behaves as one whose handler does nothing: calls return undefined and
  // construction returns the new instance.
  let callback = callback.unwrap_or(noop_function_callback);
  let fast_overloads = copy_fast_overloads(c_functions, c_functions_len);
  let constructable =
    matches!(constructor_behavior, crate::ConstructorBehavior::Allow);
  let signature = if signature_or_null.is_null() {
    ptr::null()
  } else {
    unsafe { (*(signature_or_null as *const SignatureInfo)).templ }
  };

  let data = {
    let ctx = current_ctx();
    let d = jsval_of(data_or_null);
    if !ctx.is_null() && !jsv_is_undefined(&d) {
      unsafe { JS_DupValue(ctx, d) }
    } else {
      jsv_undefined()
    }
  };
  let proto = Box::into_raw(Box::new(ObjTemplate::new(ptr::null())));
  let instance = Box::into_raw(Box::new(ObjTemplate::new(ptr::null())));
  register_template(proto as usize, TemplKind::Obj);
  register_template(instance as usize, TemplKind::Obj);
  let t = Box::into_raw(Box::new(FnTemplate {
    callback,
    data,
    constructable,
    length,
    class_name: None,
    proto,
    instance,
    parent: ptr::null(),
    signature,
    props: Vec::new(),
    accessors: Vec::new(),
    cached_proto: jsv_undefined(),
    fast_overloads,
  }));
  unsafe { (*instance).state_mut().parent_fn = t };
  register_template(t as usize, TemplKind::Func);
  t as *const FunctionTemplate
}

unsafe extern "C" fn noop_function_callback(
  _info: *const FunctionCallbackInfo,
) {
}

/// Tape replay: reconstruct a FunctionTemplate (same struct the C-ABI ctor
/// builds) with a remapped callback/data pointer.
pub(crate) fn tape_make_template(
  callback: FunctionCallback,
  data_ptr: Option<*mut c_void>,
  length: i32,
  constructable: bool,
) -> *const FunctionTemplate {
  let ctx = crate::quickjs::core::current_ctx();
  let data = match data_ptr {
    Some(p) if !p.is_null() && !ctx.is_null() => {
      make_external_jsvalue(crate::quickjs::core::current_iso(), ctx, p)
    }
    _ => jsv_undefined(),
  };
  let proto = Box::into_raw(Box::new(ObjTemplate::default_for_tape()));
  let instance = Box::into_raw(Box::new(ObjTemplate::default_for_tape()));
  register_template(proto as usize, TemplKind::Obj);
  register_template(instance as usize, TemplKind::Obj);
  let t = Box::into_raw(Box::new(FnTemplate {
    callback,
    data,
    constructable,
    length,
    class_name: None,
    proto,
    instance,
    parent: ptr::null(),
    signature: ptr::null(),
    props: Vec::new(),
    accessors: Vec::new(),
    cached_proto: jsv_undefined(),
    fast_overloads: Vec::new(),
  }));
  unsafe { (*instance).state_mut().parent_fn = t };
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
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let _tm = timing::now();
  let t = unsafe { &*(this as *const FnTemplate) };

  let f = unsafe {
    make_function_len_with_instance(
      ctx,
      t.callback,
      t.data,
      t.length,
      t.constructable && t.signature.is_null(),
      t.instance,
      t.signature,
      t.fast_overloads.clone(),
    )
  };

  if let Some(name) = &t.class_name {
    if let Ok(cname) = std::ffi::CString::new(name.as_str()) {
      const JS_PROP_CONFIGURABLE: c_int = 1 << 0;
      let nameval = unsafe { JS_NewString(ctx, cname.as_ptr()) };
      unsafe {
        JS_DefinePropertyValueStr(
          ctx,
          f,
          c"name".as_ptr(),
          nameval,
          JS_PROP_CONFIGURABLE,
        );
      }
    }
  }

  apply_props(ctx, f, &t.props);
  apply_accessors(ctx, f, &t.accessors);

  let proto_obj =
    unsafe { build_prototype_object(ctx, this as *const FnTemplate) };
  let proto_dup = unsafe { JS_DupValue(ctx, proto_obj) };
  unsafe {
    JS_SetPropertyStr(ctx, f, c"prototype".as_ptr(), proto_dup);
    JS_DefinePropertyValueStr(
      ctx,
      proto_obj,
      c"constructor".as_ptr(),
      JS_DupValue(ctx, f),
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
  }

  timing::add(&timing::GETFN_N, &timing::GETFN_T, _tm);
  let h = intern::<Function>(f);
  h
}

unsafe fn build_prototype_object(
  ctx: *mut JSContext,
  tp: *const FnTemplate,
) -> JSValue {
  let t = unsafe { &mut *(tp as *mut FnTemplate) };
  if jsv_is_object(&t.cached_proto) {
    return t.cached_proto;
  }
  if let Some(proto) = template_prototype_override(ctx, t) {
    return proto;
  }
  let _tm = timing::now();
  let proto_obj = unsafe { JS_NewObject(ctx) };
  let proto = unsafe { (&*t.proto).state() };
  if std::env::var_os("QJS_DEBUG_TMPL").is_some() {
    eprintln!(
      "[QJS build_proto] class={:?} n_props={} n_accessors={}",
      t.class_name,
      proto.props.len(),
      proto.accessors.len()
    );
  }
  if !proto.props.is_empty() {
    apply_props(ctx, proto_obj, &proto.props);
  }
  apply_accessors(ctx, proto_obj, &proto.accessors);
  if !t.parent.is_null() {
    let parent_proto = unsafe { build_prototype_object(ctx, t.parent) };
    let dup = unsafe { JS_DupValue(ctx, parent_proto) };
    unsafe { JS_SetPrototype(ctx, proto_obj, dup) };
    unsafe { JS_FreeValue(ctx, dup) };
  }

  t.cached_proto = proto_obj;
  timing::add(&timing::PROTO_N, &timing::PROTO_T, _tm);
  proto_obj
}

fn apply_props(
  ctx: *mut JSContext,
  obj: JSValue,
  props: &[(JSValue, JSValue, u32)],
) {
  timing::APPLY_N.with(|c| c.set(c.get() + 1));
  for &(key, value, attr) in props {
    if jsv_is_undefined(&key) {
      continue;
    }

    // Atom-based define so Symbol keys (e.g. a cppgc method exposed under
    // `Symbol.for("Deno_bitmapData")`) round-trip — `JS_ToCStringLen` would
    // collapse a symbol to its string description and lose it.
    let atom = unsafe { JS_ValueToAtom(ctx, key) };
    if atom == 0 {
      continue;
    }

    let (value, owned_value) = materialize_template_value(ctx, value);
    let v = unsafe { JS_DupValue(ctx, value) };
    unsafe {
      JS_DefinePropertyValue(ctx, obj, atom, v, prop_flags_from_attr(attr));
      JS_FreeAtom(ctx, atom);
      if owned_value {
        JS_FreeValue(ctx, value);
      }
    }
  }
}

fn prop_flags_from_attr(attr: u32) -> c_int {
  let mut flags = 0;
  if (attr & 1) == 0 {
    flags |= JS_PROP_WRITABLE;
  }
  if (attr & 2) == 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if (attr & 4) == 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  flags
}

fn materialize_template_value(
  ctx: *mut JSContext,
  value: JSValue,
) -> (JSValue, bool) {
  if value.tag == JS_TAG_INTRINSIC {
    let intrinsic = unsafe { value.u.int32 };
    return (materialize_intrinsic(ctx, intrinsic), true);
  }
  if value.tag != JS_TAG_TEMPLATE {
    return (value, false);
  }
  let raw = unsafe { value.u.ptr } as usize;
  let kind = TEMPLATES.with(|t| t.borrow().get(&raw).copied());
  match kind {
    Some(TemplKind::Func) => {
      let f = v8__FunctionTemplate__GetFunction(
        raw as *const FunctionTemplate,
        ctx as *const Context,
      );
      if f.is_null() {
        (value, false)
      } else {
        (jsval_of(f), false)
      }
    }
    Some(TemplKind::Obj) => {
      let o = v8__ObjectTemplate__NewInstance(
        raw as *const ObjectTemplate,
        ctx as *const Context,
      );
      if o.is_null() {
        (value, false)
      } else {
        (jsval_of(o), false)
      }
    }
    Some(TemplKind::Dict) | None => (value, false),
  }
}

fn materialize_intrinsic(ctx: *mut JSContext, intrinsic: i32) -> JSValue {
  match intrinsic {
    x if x == Intrinsic::ArrayPrototype as i32 => {
      global_prototype(ctx, c"Array".as_ptr())
    }
    x if x == Intrinsic::ArrayProtoEntries as i32 => {
      global_prototype_property(ctx, c"Array".as_ptr(), c"entries".as_ptr())
    }
    x if x == Intrinsic::ArrayProtoForEach as i32 => {
      global_prototype_property(ctx, c"Array".as_ptr(), c"forEach".as_ptr())
    }
    x if x == Intrinsic::ArrayProtoKeys as i32 => {
      global_prototype_property(ctx, c"Array".as_ptr(), c"keys".as_ptr())
    }
    x if x == Intrinsic::ArrayProtoValues as i32 => {
      global_prototype_property(ctx, c"Array".as_ptr(), c"values".as_ptr())
    }
    x if x == Intrinsic::ErrorPrototype as i32 => {
      global_prototype(ctx, c"Error".as_ptr())
    }
    x if x == Intrinsic::ObjProtoValueOf as i32 => {
      global_prototype_property(ctx, c"Object".as_ptr(), c"valueOf".as_ptr())
    }
    _ => jsv_undefined(),
  }
}

fn global_prototype(ctx: *mut JSContext, ctor_name: *const c_char) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, ctor_name);
    JS_FreeValue(ctx, global);
    if !jsv_is_object(&ctor) {
      if ctor.tag != JS_TAG_EXCEPTION {
        JS_FreeValue(ctx, ctor);
      }
      return jsv_undefined();
    }
    let proto = JS_GetPropertyStr(ctx, ctor, c"prototype".as_ptr());
    JS_FreeValue(ctx, ctor);
    if proto.tag == JS_TAG_EXCEPTION {
      jsv_undefined()
    } else {
      proto
    }
  }
}

fn global_prototype_property(
  ctx: *mut JSContext,
  ctor_name: *const c_char,
  prop_name: *const c_char,
) -> JSValue {
  let proto = global_prototype(ctx, ctor_name);
  if !jsv_is_object(&proto) {
    return jsv_undefined();
  }
  unsafe {
    let value = JS_GetPropertyStr(ctx, proto, prop_name);
    JS_FreeValue(ctx, proto);
    if value.tag == JS_TAG_EXCEPTION {
      jsv_undefined()
    } else {
      value
    }
  }
}

fn template_prototype_override(
  ctx: *mut JSContext,
  t: &mut FnTemplate,
) -> Option<JSValue> {
  for idx in 0..t.props.len() {
    let (key, value, _) = t.props[idx];
    if !template_key_is(ctx, key, b"prototype") {
      continue;
    }

    let (proto, owned_proto) = materialize_template_value(ctx, value);
    if jsv_is_object(&proto) {
      let cached = if owned_proto {
        proto
      } else {
        unsafe { JS_DupValue(ctx, proto) }
      };
      t.cached_proto = cached;
      return Some(cached);
    }
    if owned_proto {
      unsafe { JS_FreeValue(ctx, proto) };
    }
  }
  None
}

fn template_key_is(ctx: *mut JSContext, key: JSValue, expected: &[u8]) -> bool {
  if ctx.is_null() {
    return false;
  }
  let mut len = 0usize;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, key) };
  if s.is_null() {
    return false;
  }
  let ok = unsafe { slice::from_raw_parts(s as *const u8, len) } == expected;
  unsafe { JS_FreeCString(ctx, s) };
  ok
}

fn intrinsic_template_value(intrinsic: Intrinsic) -> JSValue {
  make_value(
    JS_TAG_INTRINSIC,
    JSValueUnion {
      int32: intrinsic as i32,
    },
  )
}

fn store_intrinsic_template_property(
  this: *const crate::Template,
  key: *const Name,
  intrinsic: Intrinsic,
  attr: PropertyAttribute,
) {
  if this.is_null() {
    return;
  }

  let raw = this as *const c_void as usize;
  let ctx = current_ctx();
  let key_owned = own_template_value(ctx, jsval_of(key));
  let stored = intrinsic_template_value(intrinsic);
  with_template_props(raw, |props| {
    props.push((key_owned, stored, attr.as_u32_lenient()));
  });
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
  let mut len: usize = 0;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, jsval_of(name)) };
  if s.is_null() {
    return;
  }
  let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
  if let Ok(name) = std::str::from_utf8(bytes) {
    t.class_name = Some(name.to_owned());
  }
  unsafe { JS_FreeCString(ctx, s) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__New(
  isolate: *mut RealIsolate,
  templ: *const FunctionTemplate,
) -> *const ObjectTemplate {
  let _ = isolate;
  let t = Box::into_raw(Box::new(ObjTemplate::new(templ as *const FnTemplate)));
  register_template(t as usize, TemplKind::Obj);
  t as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__NewInstance(
  this: *const ObjectTemplate,
  context: *const Context,
) -> *const Object {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let _tm = timing::now();
  if !this.is_null() {
    let t = unsafe { (&*(this as *const ObjTemplate)).state() };
    let obj = new_object_for_template(ctx, t);
    if jsv_is_exception(&obj) {
      return ptr::null();
    }
    super::object::set_internal_field_count_for_value(
      obj,
      t.internal_field_count,
    );
    if !t.parent_fn.is_null() {
      let _ = v8__FunctionTemplate__GetFunction(
        t.parent_fn as *const FunctionTemplate,
        context,
      );
      let proto_obj = unsafe { build_prototype_object(ctx, t.parent_fn) };
      let dup = unsafe { JS_DupValue(ctx, proto_obj) };
      unsafe { JS_SetPrototype(ctx, obj, dup) };
      unsafe { JS_FreeValue(ctx, dup) };
      super::object::mark_api_wrapper_for_value(obj);
    }
    apply_props(ctx, obj, &t.props);
    apply_accessors(ctx, obj, &t.accessors);
    if t.immutable_proto {
      unsafe { JS_PreventExtensions(ctx, obj) };
    }
    timing::add(&timing::NEWINST_N, &timing::NEWINST_T, _tm);
    return intern::<Object>(obj);
  }
  timing::add(&timing::NEWINST_N, &timing::NEWINST_T, _tm);
  let obj = unsafe { JS_NewObject(ctx) };
  intern::<Object>(obj)
}

fn install_named_global_handler(
  ctx: *mut JSContext,
  global: JSValue,
  handler: &NamedHandler,
) {
  install_global_define_property_handler(ctx, global, handler);

  if handler.getter.is_none() || jsv_is_undefined(&handler.data) {
    return;
  }

  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      handler.data,
      JS_GPN_STRING_MASK_QJS,
    )
  };
  if rc < 0 {
    return;
  }

  for i in 0..len {
    let atom = unsafe { (*tab.add(i as usize)).atom };
    let has = unsafe { JS_HasProperty(ctx, global, atom) };
    if has != 0 {
      continue;
    }

    let magic = register_named_getter(ctx, handler, atom);
    let getter = unsafe {
      make_cfunc_magic(
        ctx,
        named_getter_trampoline,
        ptr::null(),
        0,
        JS_CFUNC_GENERIC_MAGIC,
        magic,
      )
    };
    unsafe {
      JS_DefinePropertyGetSet(
        ctx,
        global,
        atom,
        getter,
        jsv_undefined(),
        JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE,
      );
    }
  }

  unsafe { JS_FreePropertyEnum(ctx, tab, len) };
}

fn install_global_define_property_handler(
  ctx: *mut JSContext,
  global: JSValue,
  handler: &NamedHandler,
) {
  if handler.setter.is_none()
    && handler.definer.is_none()
    && handler.descriptor.is_none()
  {
    return;
  }

  unsafe {
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    if !jsv_is_object(&object_ctor) {
      JS_FreeValue(ctx, object_ctor);
      return;
    }
    let original =
      JS_GetPropertyStr(ctx, object_ctor, c"defineProperty".as_ptr());
    if !jsv_is_object(&original) {
      JS_FreeValue(ctx, original);
      JS_FreeValue(ctx, object_ctor);
      return;
    }

    let magic = register_global_define(GlobalDefineEntry {
      original,
      object_ctor,
      global: JS_DupValue(ctx, global),
      handler: clone_named_handler(ctx, handler),
    });
    let wrapper = make_cfunc_magic(
      ctx,
      global_define_property_trampoline,
      c"defineProperty".as_ptr(),
      3,
      JS_CFUNC_GENERIC_MAGIC,
      magic,
    );
    JS_SetPropertyStr(ctx, object_ctor, c"defineProperty".as_ptr(), wrapper);
  }
}

pub(crate) fn apply_object_template_to_global(
  ctx: *mut JSContext,
  global: JSValue,
  templ: *const ObjectTemplate,
) {
  if ctx.is_null() || templ.is_null() {
    return;
  }
  let t = unsafe { (&*(templ as *const ObjTemplate)).state() };
  super::object::set_internal_field_count_for_value(
    global,
    t.internal_field_count,
  );
  apply_props(ctx, global, &t.props);
  apply_accessors(ctx, global, &t.accessors);
  let isolate = current_iso();
  let is_snapshot_creator =
    !isolate.is_null() && iso_state(isolate).is_snapshot_creator;
  if !is_snapshot_creator {
    if let Some(handler) = t.named_handler.as_ref() {
      install_named_global_handler(ctx, global, handler);
    }
    if t.named_handler.is_some() || t.indexed_handler.is_some() {
      install_global_handler_proxy(
        ctx,
        global,
        NamedHandlerInstance {
          named_handler: t
            .named_handler
            .as_ref()
            .map(|handler| clone_named_handler(ctx, handler)),
          indexed_handler: t
            .indexed_handler
            .as_ref()
            .map(|handler| clone_indexed_handler(ctx, handler)),
        },
      );
    }
  }
  if t.immutable_proto {
    unsafe { JS_PreventExtensions(ctx, global) };
  }
}

fn install_global_handler_proxy(
  ctx: *mut JSContext,
  global: JSValue,
  instance: NamedHandlerInstance,
) {
  let class_id = named_handler_class_id_current();
  if class_id == 0 {
    free_named_handler_instance(ctx, instance);
    return;
  }
  let proxy = unsafe { JS_NewObjectClass(ctx, class_id as c_int) };
  if jsv_is_exception(&proxy) {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    free_named_handler_instance(ctx, instance);
    return;
  }
  unsafe { JS_SetOpaque(proxy, Box::into_raw(Box::new(instance)).cast()) };
  let old_proto = unsafe { JS_GetPrototype(ctx, global) };
  if jsv_is_exception(&old_proto) {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe {
      JS_FreeValue(ctx, exc);
      JS_FreeValue(ctx, proxy);
    }
    return;
  }
  unsafe {
    JS_SetPrototype(ctx, proxy, old_proto);
    JS_SetPrototype(ctx, global, proxy);
    JS_FreeValue(ctx, old_proto);
    JS_FreeValue(ctx, proxy);
  }
}

fn free_named_handler_instance(
  ctx: *mut JSContext,
  instance: NamedHandlerInstance,
) {
  if let Some(handler) = instance.named_handler
    && !jsv_is_undefined(&handler.data)
  {
    unsafe { JS_FreeValue(ctx, handler.data) };
  }
  if let Some(handler) = instance.indexed_handler
    && !jsv_is_undefined(&handler.data)
  {
    unsafe { JS_FreeValue(ctx, handler.data) };
  }
}

pub(crate) fn snapshot_object_template_info(
  templ: *const ObjectTemplate,
) -> Option<SnapshotObjectTemplateInfo> {
  let templ = unsafe { (templ as *const ObjTemplate).as_ref()?.state() };
  Some(SnapshotObjectTemplateInfo {
    internal_field_count: templ.internal_field_count,
    named_handler: templ.named_handler.as_ref().map(|handler| {
      SnapshotNamedHandlerInfo {
        getter: handler.getter,
        setter: handler.setter,
        query: handler.query,
        deleter: handler.deleter,
        enumerator: handler.enumerator,
        definer: handler.definer,
        descriptor: handler.descriptor,
        data: handler.data,
        owner_ctx: handler.owner_ctx,
        non_masking: handler.non_masking,
        only_intercept_strings: handler.only_intercept_strings,
      }
    }),
    indexed_handler: templ.indexed_handler.as_ref().map(|handler| {
      SnapshotIndexedHandlerInfo {
        getter: handler.getter,
        setter: handler.setter,
        query: handler.query,
        deleter: handler.deleter,
        enumerator: handler.enumerator,
        definer: handler.definer,
        descriptor: handler.descriptor,
        data: handler.data,
        owner_ctx: handler.owner_ctx,
        non_masking: handler.non_masking,
      }
    }),
  })
}

pub(crate) fn restore_snapshot_object_template(
  ctx: *mut JSContext,
  global: JSValue,
  info: SnapshotObjectTemplateInfo,
) {
  super::object::set_internal_field_count_for_value(
    global,
    info.internal_field_count,
  );
  let named_handler = info.named_handler.map(|handler| NamedHandler {
    getter: handler.getter,
    setter: handler.setter,
    query: handler.query,
    deleter: handler.deleter,
    enumerator: handler.enumerator,
    definer: handler.definer,
    descriptor: handler.descriptor,
    data: handler.data,
    owner_ctx: ctx,
    non_masking: handler.non_masking,
    only_intercept_strings: handler.only_intercept_strings,
  });
  let indexed_handler = info.indexed_handler.map(|handler| IndexedHandler {
    getter: handler.getter,
    setter: handler.setter,
    query: handler.query,
    deleter: handler.deleter,
    enumerator: handler.enumerator,
    definer: handler.definer,
    descriptor: handler.descriptor,
    data: handler.data,
    owner_ctx: ctx,
    non_masking: handler.non_masking,
  });
  if named_handler.is_some() || indexed_handler.is_some() {
    install_global_handler_proxy(
      ctx,
      global,
      NamedHandlerInstance {
        named_handler,
        indexed_handler,
      },
    );
  }
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
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  // Own the key for the accessor's lifetime — see `own_template_value` in
  // `v8__Template__Set`: the borrowed handle would otherwise dangle.
  let key_owned = own_template_value(current_ctx(), jsval_of(key));
  t.accessors.push(TemplAccessor {
    key: key_owned,
    getter: getter as *const FnTemplate,
    setter: setter as *const FnTemplate,
    native_getter: None,
    native_setter: None,
    data: jsv_undefined(),
    attr: attr.as_u32_lenient(),
    lazy: false,
  });
}

fn apply_accessors(
  ctx: *mut JSContext,
  obj: JSValue,
  accessors: &[TemplAccessor],
) {
  if accessors.is_empty() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    JS_FreeValue(ctx, global);
    if !jsv_is_object(&object_ctor) {
      JS_FreeValue(ctx, object_ctor);
      return;
    }
    let define =
      JS_GetPropertyStr(ctx, object_ctor, c"defineProperty".as_ptr());
    if !jsv_is_object(&define) {
      JS_FreeValue(ctx, define);
      JS_FreeValue(ctx, object_ctor);
      return;
    }

    for acc in accessors {
      if jsv_is_undefined(&acc.key) {
        continue;
      }
      if let Some(native_getter) = acc.native_getter {
        let _ = super::object::define_native_accessor_value(
          ctx,
          obj,
          acc.key,
          native_getter,
          acc.native_setter,
          acc.data,
          acc.attr,
          acc.lazy,
        );
        continue;
      }
      let desc = JS_NewObject(ctx);
      if !acc.getter.is_null() {
        let t = &*acc.getter;
        let gf = make_function_len(ctx, t.callback, t.data, t.length, false);
        JS_SetPropertyStr(ctx, desc, c"get".as_ptr(), gf);
      }
      if !acc.setter.is_null() {
        let t = &*acc.setter;
        let sf = make_function_len(ctx, t.callback, t.data, t.length, false);
        JS_SetPropertyStr(ctx, desc, c"set".as_ptr(), sf);
      }

      let enumerable = (acc.attr & 2) == 0;
      let configurable = (acc.attr & 4) == 0;
      JS_SetPropertyStr(
        ctx,
        desc,
        c"enumerable".as_ptr(),
        jsv_bool(enumerable),
      );
      JS_SetPropertyStr(
        ctx,
        desc,
        c"configurable".as_ptr(),
        jsv_bool(configurable),
      );

      let mut args = [JS_DupValue(ctx, obj), JS_DupValue(ctx, acc.key), desc];
      let r = JS_Call(ctx, define, object_ctor, 3, args.as_mut_ptr());
      JS_FreeValue(ctx, args[0]);
      JS_FreeValue(ctx, args[1]);
      JS_FreeValue(ctx, args[2]);
      if !jsv_is_exception(&r) {
        JS_FreeValue(ctx, r);
      }
    }

    JS_FreeValue(ctx, define);
    JS_FreeValue(ctx, object_ctor);
  }
}

// rusty_v8's opaque API types are zero-sized. Keep handler mutation behind a
// real ABI boundary so optimized Rust code cannot discard the pointer identity.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn v8__ObjectTemplate__SetIndexedPropertyHandler(
  this: *const c_void,
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
  if this.is_null() {
    return;
  }
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    own_template_value(current_ctx(), jsval_of(data_or_null))
  };
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  t.indexed_handler = Some(IndexedHandler {
    getter,
    setter,
    query,
    deleter,
    enumerator,
    definer,
    descriptor,
    data,
    owner_ctx: current_ctx(),
    non_masking: flags.is_non_masking(),
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetInternalFieldCount(
  this: *const ObjectTemplate,
  value: crate::support::int,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  t.internal_field_count = value;
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn v8__ObjectTemplate__SetNamedPropertyHandler(
  this: *const c_void,
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
  if this.is_null() {
    return;
  }
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    own_template_value(current_ctx(), jsval_of(data_or_null))
  };
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  t.named_handler = Some(NamedHandler {
    getter,
    setter,
    query,
    deleter,
    enumerator,
    definer,
    descriptor,
    data,
    owner_ctx: current_ctx(),
    non_masking: flags.is_non_masking(),
    only_intercept_strings: flags.is_only_intercept_strings(),
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut RealIsolate {
  unsafe { prop_cbinfo(this) }
    .map(|info| info.isolate)
    .unwrap_or_else(current_iso)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetReturnValue(
  this: *const c_void,
) -> usize {
  if this.is_null() {
    return property_return_scratch();
  }
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return property_return_scratch();
  };
  (&mut *info.return_slot as *mut JSValue) as usize
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Holder(
  this: *const c_void,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return ptr::null();
  };
  intern_dup::<Object>(info.ctx, info.this)
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
  this: *const std::os::raw::c_void,
) -> bool {
  if this.is_null() {
    return false;
  }
  cbinfo(this as *const FunctionCallbackInfo).is_construct
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetAccessorProperty(
  this: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  getter: *const std::os::raw::c_void,
  setter: *const std::os::raw::c_void,
  attr: crate::PropertyAttribute,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  let key_owned = own_template_value(current_ctx(), jsval_of(key));
  t.accessors.push(TemplAccessor {
    key: key_owned,
    getter: getter as *const FnTemplate,
    setter: setter as *const FnTemplate,
    native_getter: None,
    native_setter: None,
    data: jsv_undefined(),
    attr: attr.as_u32_lenient(),
    lazy: false,
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetName(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  if this.is_null() {
    let empty = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
    return intern::<String>(empty) as *const std::os::raw::c_void;
  }

  let name = unsafe {
    JS_GetPropertyStr(ctx, jsval_of(this as *const Function), c"name".as_ptr())
  };
  if jsv_is_string(&name) {
    return intern::<String>(name) as *const std::os::raw::c_void;
  }
  unsafe { JS_FreeValue(ctx, name) };
  let empty = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
  intern::<String>(empty) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptColumnNumber(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  function_script_info(this)
    .map(|info| info.column)
    .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptLineNumber(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  function_script_info(this)
    .map(|info| info.line)
    .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptOrigin(
  this: *const std::os::raw::c_void,
  out: *mut std::os::raw::c_void,
) {
  if out.is_null() {
    return;
  }
  unsafe {
    ptr::write_bytes(
      out as *mut u8,
      0u8,
      std::mem::size_of::<crate::ScriptOrigin>(),
    );
  }
  let Some(info) = function_script_info(this) else {
    return;
  };
  let ctx = current_ctx();
  let raw = out as *mut RawScriptOrigin;
  unsafe {
    (*raw).script_id = info.script_id;
  }
  if let Some(resource_name) = info.resource_name.as_deref() {
    unsafe {
      (*raw).resource_name = origin_string_slot(ctx, resource_name);
    }
  }
  if let Some(source_map_url) = info.source_map_url.as_deref() {
    unsafe {
      (*raw).source_map_url = origin_string_slot(ctx, source_map_url);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__ScriptId(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  function_script_info(this)
    .map(|info| info.script_id)
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__InternalFieldCount(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  let t = unsafe { (&*(this as *const ObjTemplate)).state() };
  t.internal_field_count
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
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  // The data outlives the call that set it, so the template owns a reference
  // to it, the same way `FunctionTemplate` does.
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    let ctx = current_ctx();
    let raw = jsval_of(data_or_null);
    if ctx.is_null() {
      raw
    } else {
      unsafe { JS_DupValue(ctx, raw) }
    }
  };
  t.call_handler = Some((callback, data));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetImmutableProto(
  this: *const std::os::raw::c_void,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  t.immutable_proto = true;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNativeDataProperty(
  this: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const std::os::raw::c_void,
  attr: crate::PropertyAttribute,
) {
  if this.is_null() || key.is_null() {
    return;
  }
  let ctx = current_ctx();
  let key_owned = own_template_value(ctx, jsval_of(key));
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    own_template_value(ctx, jsval_of(data_or_null))
  };
  let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
  t.accessors.push(TemplAccessor {
    key: key_owned,
    getter: ptr::null(),
    setter: ptr::null(),
    native_getter: Some(getter),
    native_setter: setter,
    data,
    attr: attr.as_u32_lenient(),
    lazy: false,
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Data(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return std::ptr::null();
  };
  intern_dup::<Value>(info.ctx, info.data) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Get(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let slot = unsafe { rv_slot(this as *mut RawReturnValue) };
  if slot.is_null() {
    return ptr::null();
  }
  let value = unsafe { *slot };
  intern_dup::<Value>(current_ctx(), value) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetEmptyString(
  this: *mut std::os::raw::c_void,
) {
  let slot = unsafe { rv_slot(this as *mut RawReturnValue) };
  let ctx = current_ctx();
  if !slot.is_null() && !ctx.is_null() {
    unsafe {
      *slot = JS_NewStringLen(ctx, c"".as_ptr(), 0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Signature__New(
  _isolate: *mut std::os::raw::c_void,
  templ: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  if templ.is_null() {
    return std::ptr::null();
  }
  Box::into_raw(Box::new(SignatureInfo {
    templ: templ as *const FnTemplate,
  })) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetIntrinsicDataProperty(
  this: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  intrinsic: crate::Intrinsic,
  attr: crate::PropertyAttribute,
) {
  store_intrinsic_template_property(
    this as *const crate::Template,
    key as *const Name,
    intrinsic,
    attr,
  );
}

fn push_template_accessor(this: *const c_void, accessor: TemplAccessor) {
  match template_kind(this) {
    Some(TemplKind::Func) => {
      let t = unsafe { &mut *(this as *mut FnTemplate) };
      t.accessors.push(accessor);
    }
    Some(TemplKind::Obj) => {
      let t = unsafe { (&*(this as *const ObjTemplate)).state_mut() };
      t.accessors.push(accessor);
    }
    Some(TemplKind::Dict) | None => {}
  }
}

fn native_template_accessor(
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const Value,
  attr: PropertyAttribute,
  lazy: bool,
) -> TemplAccessor {
  let ctx = current_ctx();
  TemplAccessor {
    key: own_template_value(ctx, jsval_of(key)),
    getter: ptr::null(),
    setter: ptr::null(),
    native_getter: Some(getter),
    native_setter: setter,
    data: if data_or_null.is_null() {
      jsv_undefined()
    } else {
      own_template_value(ctx, jsval_of(data_or_null))
    },
    attr: attr.as_u32_lenient(),
    lazy,
  }
}

/// `Template::SetNativeDataProperty`: on an ObjectTemplate the accessor lands
/// on every instance, on a FunctionTemplate on the function itself.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetNativeDataProperty(
  this: *const crate::Template,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  setter: Option<crate::AccessorNameSetterCallback>,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) {
  if this.is_null() || key.is_null() {
    return;
  }
  let accessor =
    native_template_accessor(key, getter, setter, data_or_null, attr, false);
  push_template_accessor(this as *const c_void, accessor);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetLazyDataProperty(
  this: *const crate::Template,
  key: *const Name,
  getter: crate::AccessorNameGetterCallback,
  data_or_null: *const Value,
  attr: PropertyAttribute,
) {
  if this.is_null() || key.is_null() {
    return;
  }
  let accessor =
    native_template_accessor(key, getter, None, data_or_null, attr, true);
  push_template_accessor(this as *const c_void, accessor);
}

/// Whether `value` was constructed by the function of `templ` or of a
/// template that inherits from it. Construction through a template function
/// marks the instance as an API wrapper and gives it the template's
/// prototype, so both are required.
pub(crate) fn template_has_instance(
  ctx: *mut JSContext,
  value: JSValue,
  templ: *const FunctionTemplate,
) -> bool {
  template_kind(templ as *const c_void) == Some(TemplKind::Func)
    && jsv_is_object(&value)
    && super::object::is_api_wrapper_value(value)
    && receiver_matches_signature(ctx, value, templ as *const FnTemplate)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__HasInstance(
  this: *const FunctionTemplate,
  object: *const Value,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || object.is_null() {
    return false;
  }
  template_has_instance(ctx, jsval_of(object), this)
}

/// `v8::DictionaryTemplate`: a fixed list of property names, filled in with
/// values at instantiation. The leading slot reads as `undefined` through
/// `jsval_of`, so code that peeks at a handle's JSValue sees a harmless value.
struct DictTemplate {
  _slot: JSValue,
  names: Vec<std::string::String>,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DictionaryTemplate__New(
  _isolate: *mut RealIsolate,
  names: *const *const c_char,
  name_lengths: *const usize,
  names_len: usize,
) -> *const crate::DictionaryTemplate {
  let names = (0..names_len)
    .map(|i| unsafe {
      let bytes = std::slice::from_raw_parts(
        *names.add(i) as *const u8,
        *name_lengths.add(i),
      );
      std::string::String::from_utf8_lossy(bytes).into_owned()
    })
    .collect();
  let t = Box::into_raw(Box::new(DictTemplate {
    _slot: jsv_undefined(),
    names,
  }));
  register_template(t as usize, TemplKind::Dict);
  t as *const crate::DictionaryTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DictionaryTemplate__NewInstance(
  this: *const crate::DictionaryTemplate,
  context: *const Context,
  values: *const *const Value,
  values_len: usize,
) -> *const Object {
  let ctx = ctx_of(context);
  if ctx.is_null()
    || template_kind(this as *const c_void) != Some(TemplKind::Dict)
  {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const DictTemplate) };
  let obj = unsafe { JS_NewObject(ctx) };
  if jsv_is_exception(&obj) {
    return ptr::null();
  }
  for (i, name) in t.names.iter().enumerate().take(values_len) {
    let value = unsafe { *values.add(i) };
    // A missing value means the property is left out entirely.
    if value.is_null() {
      continue;
    }
    let Ok(cname) = std::ffi::CString::new(name.as_str()) else {
      continue;
    };
    unsafe {
      JS_DefinePropertyValueStr(
        ctx,
        obj,
        cname.as_ptr(),
        JS_DupValue(ctx, jsval_of(value)),
        JS_PROP_C_W_E,
      );
    }
  }
  intern::<Object>(obj)
}
