#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]

use core::ffi::c_char;
use core::ffi::c_int;
use core::ffi::c_void;

#[repr(C)]
pub struct JSRuntime {
  _private: [u8; 0],
}

#[repr(C)]
pub struct JSSharedArrayBufferFunctions {
  pub sab_alloc: Option<
    unsafe extern "C" fn(opaque: *mut c_void, size: usize) -> *mut c_void,
  >,
  pub sab_free:
    Option<unsafe extern "C" fn(opaque: *mut c_void, ptr: *mut c_void)>,
  pub sab_dup:
    Option<unsafe extern "C" fn(opaque: *mut c_void, ptr: *mut c_void)>,
  pub sab_opaque: *mut c_void,
  pub sab_alloc_zeroed: bool,
}

#[repr(C)]
pub struct JSContext {
  _private: [u8; 0],
}

#[repr(C)]
pub struct JSModuleDef {
  _private: [u8; 0],
}

#[repr(C)]
pub struct JSClass {
  _private: [u8; 0],
}

pub type JSClassID = u32;
pub type JSAtom = u32;

#[repr(C)]
#[derive(Copy, Clone)]
pub union JSValueUnion {
  pub int32: i32,
  pub float64: f64,
  pub ptr: *mut c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct JSValue {
  pub u: JSValueUnion,
  pub tag: i64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct JSMemoryUsage {
  pub malloc_size: i64,
  pub malloc_limit: i64,
  pub memory_used_size: i64,
  pub malloc_count: i64,
  pub memory_used_count: i64,
  pub atom_count: i64,
  pub atom_size: i64,
  pub str_count: i64,
  pub str_size: i64,
  pub obj_count: i64,
  pub obj_size: i64,
  pub prop_count: i64,
  pub prop_size: i64,
  pub shape_count: i64,
  pub shape_size: i64,
  pub js_func_count: i64,
  pub js_func_size: i64,
  pub js_func_code_size: i64,
  pub js_func_pc2line_count: i64,
  pub js_func_pc2line_size: i64,
  pub c_func_count: i64,
  pub array_count: i64,
  pub fast_array_count: i64,
  pub fast_array_elements: i64,
  pub binary_object_count: i64,
  pub binary_object_size: i64,
}

impl JSValue {
  pub fn as_ptr(&self) -> *const Self {
    self as *const Self
  }
}

pub const JS_TAG_FIRST: i64 = -9;
pub const JS_TAG_BIG_INT: i64 = -9;
pub const JS_TAG_SYMBOL: i64 = -8;
pub const JS_TAG_STRING: i64 = -7;
pub const JS_TAG_STRING_ROPE: i64 = -6;
pub const JS_TAG_MODULE: i64 = -3;
pub const JS_TAG_FUNCTION_BYTECODE: i64 = -2;
pub const JS_TAG_OBJECT: i64 = -1;
pub const JS_TAG_INT: i64 = 0;
pub const JS_TAG_BOOL: i64 = 1;
pub const JS_TAG_NULL: i64 = 2;
pub const JS_TAG_UNDEFINED: i64 = 3;
pub const JS_TAG_UNINITIALIZED: i64 = 4;
pub const JS_TAG_CATCH_OFFSET: i64 = 5;
pub const JS_TAG_EXCEPTION: i64 = 6;
pub const JS_TAG_SHORT_BIG_INT: i64 = 7;
pub const JS_TAG_FLOAT64: i64 = 8;

#[inline]
pub const fn make_value(tag: i64, u: JSValueUnion) -> JSValue {
  JSValue { u, tag }
}

#[inline]
pub fn jsv_undefined() -> JSValue {
  make_value(JS_TAG_UNDEFINED, JSValueUnion { int32: 0 })
}
#[inline]
pub fn jsv_null() -> JSValue {
  make_value(JS_TAG_NULL, JSValueUnion { int32: 0 })
}
#[inline]
pub fn jsv_bool(b: bool) -> JSValue {
  make_value(
    JS_TAG_BOOL,
    JSValueUnion {
      int32: if b { 1 } else { 0 },
    },
  )
}
#[inline]
pub fn jsv_int32(v: i32) -> JSValue {
  make_value(JS_TAG_INT, JSValueUnion { int32: v })
}
#[inline]
pub fn jsv_float64(v: f64) -> JSValue {
  make_value(JS_TAG_FLOAT64, JSValueUnion { float64: v })
}
#[inline]
pub fn jsv_exception() -> JSValue {
  make_value(JS_TAG_EXCEPTION, JSValueUnion { int32: 0 })
}

#[inline]
pub fn jsv_is_undefined(v: &JSValue) -> bool {
  v.tag == JS_TAG_UNDEFINED
}
#[inline]
pub fn jsv_is_null(v: &JSValue) -> bool {
  v.tag == JS_TAG_NULL
}
#[inline]
pub fn jsv_is_bool(v: &JSValue) -> bool {
  v.tag == JS_TAG_BOOL
}
#[inline]
pub fn jsv_is_int(v: &JSValue) -> bool {
  v.tag == JS_TAG_INT
}
#[inline]
pub fn jsv_is_float64(v: &JSValue) -> bool {
  v.tag == JS_TAG_FLOAT64
}
#[inline]
pub fn jsv_is_number(v: &JSValue) -> bool {
  v.tag == JS_TAG_INT || v.tag == JS_TAG_FLOAT64
}
#[inline]
pub fn jsv_is_string(v: &JSValue) -> bool {
  v.tag == JS_TAG_STRING || v.tag == JS_TAG_STRING_ROPE
}
#[inline]
pub fn jsv_is_symbol(v: &JSValue) -> bool {
  v.tag == JS_TAG_SYMBOL
}
#[inline]
pub fn jsv_is_object(v: &JSValue) -> bool {
  v.tag == JS_TAG_OBJECT
}
#[inline]
pub fn jsv_is_bigint(v: &JSValue) -> bool {
  v.tag == JS_TAG_BIG_INT || v.tag == JS_TAG_SHORT_BIG_INT
}
#[inline]
pub fn jsv_is_exception(v: &JSValue) -> bool {
  v.tag == JS_TAG_EXCEPTION
}

#[inline]
pub fn jsv_get_ptr(v: &JSValue) -> *mut c_void {
  unsafe { v.u.ptr }
}

pub const JS_EVAL_TYPE_GLOBAL: c_int = 0;
pub const JS_EVAL_TYPE_MODULE: c_int = 1;
pub const JS_EVAL_TYPE_DIRECT: c_int = 2;
pub const JS_EVAL_TYPE_INDIRECT: c_int = 3;
pub const JS_EVAL_TYPE_MASK: c_int = 3;
pub const JS_EVAL_FLAG_STRICT: c_int = 1 << 3;
pub const JS_EVAL_FLAG_COMPILE_ONLY: c_int = 1 << 5;
pub const JS_EVAL_FLAG_BACKTRACE_BARRIER: c_int = 1 << 6;
pub const JS_EVAL_FLAG_ASYNC: c_int = 1 << 7;

pub const JS_PROP_CONFIGURABLE: c_int = 1 << 0;
pub const JS_PROP_WRITABLE: c_int = 1 << 1;
pub const JS_PROP_ENUMERABLE: c_int = 1 << 2;
pub const JS_PROP_C_W_E: c_int =
  JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE | JS_PROP_ENUMERABLE;
pub const JS_PROP_THROW: c_int = 1 << 14;
pub const JS_PROP_THROW_STRICT: c_int = 1 << 15;

pub const JS_PROMISE_HOOK_INIT: c_int = 0;
pub const JS_PROMISE_HOOK_BEFORE: c_int = 1;
pub const JS_PROMISE_HOOK_AFTER: c_int = 2;
pub const JS_PROMISE_HOOK_RESOLVE: c_int = 3;

pub type JSCFunction = unsafe extern "C" fn(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue;

pub type JSCFunctionData = unsafe extern "C" fn(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
  func_data: *mut JSValue,
) -> JSValue;

pub type JSModuleInitFunc =
  unsafe extern "C" fn(ctx: *mut JSContext, m: *mut JSModuleDef) -> c_int;
pub type JSModuleNormalizeFunc = unsafe extern "C" fn(
  ctx: *mut JSContext,
  module_base_name: *const c_char,
  module_name: *const c_char,
  opaque: *mut c_void,
) -> *mut c_char;

pub type JSModuleLoaderFunc = unsafe extern "C" fn(
  ctx: *mut JSContext,
  module_name: *const c_char,
  opaque: *mut c_void,
) -> *mut JSModuleDef;

pub type JSModuleLoaderFunc2 = unsafe extern "C" fn(
  ctx: *mut JSContext,
  module_name: *const c_char,
  opaque: *mut c_void,
  attributes: JSValue,
) -> *mut JSModuleDef;

pub type JSModuleCheckSupportedImportAttributes = unsafe extern "C" fn(
  ctx: *mut JSContext,
  opaque: *mut c_void,
  attributes: JSValue,
)
  -> c_int;

pub type JSV82jscDynImportHook = unsafe extern "C" fn(
  ctx: *mut JSContext,
  function: JSValue,
  basename: JSValue,
  specifier: JSValue,
  attributes: JSValue,
  resolving_funcs: *const JSValue,
);

pub type JSV82jscImportMetaHook = unsafe extern "C" fn(
  ctx: *mut JSContext,
  module: *mut JSModuleDef,
  meta: JSValue,
) -> c_int;

pub type JSHostPromiseRejectionTracker = unsafe extern "C" fn(
  ctx: *mut JSContext,
  promise: JSValue,
  reason: JSValue,
  is_handled: c_int,
  opaque: *mut c_void,
);

// `typedef int JSInterruptHandler(JSRuntime *rt, void *opaque);`
// Return != 0 to interrupt the running JS.
pub type JSInterruptHandler =
  unsafe extern "C" fn(rt: *mut JSRuntime, opaque: *mut c_void) -> c_int;
pub type JSMemoryLimitHandler =
  unsafe extern "C" fn(rt: *mut JSRuntime, opaque: *mut c_void);
pub type JSStackFrameVisitor = unsafe extern "C" fn(
  opaque: *mut c_void,
  function_name: *const c_char,
  filename: *const c_char,
  line_num: c_int,
  column_num: c_int,
);

unsafe extern "C" {
  pub fn JS_GetVersion() -> *const std::os::raw::c_char;
  pub fn JS_NewRuntime() -> *mut JSRuntime;
  pub fn JS_FreeRuntime(rt: *mut JSRuntime);
  pub fn JS_SetRuntimeOpaque(rt: *mut JSRuntime, opaque: *mut c_void);
  pub fn JS_GetRuntimeOpaque(rt: *mut JSRuntime) -> *mut c_void;
  pub fn JS_SetMemoryLimit(rt: *mut JSRuntime, limit: usize);
  pub fn JS_SetMemoryLimitHandler(
    rt: *mut JSRuntime,
    handler: Option<JSMemoryLimitHandler>,
    opaque: *mut c_void,
  );
  pub fn JS_SetMaxStackSize(rt: *mut JSRuntime, stack_size: usize);
  /// Re-reads the current thread's stack bounds into the runtime. Needed when
  /// a runtime moves between threads under `v8::Locker` (see `locker.rs`).
  pub fn JS_UpdateStackTop(rt: *mut JSRuntime);
  pub fn JS_SetCanBlock(rt: *mut JSRuntime, can_block: bool);
  pub fn JS_ComputeMemoryUsage(rt: *mut JSRuntime, usage: *mut JSMemoryUsage);
  pub fn JS_SetSharedArrayBufferFunctions(
    rt: *mut JSRuntime,
    sf: *const JSSharedArrayBufferFunctions,
  );
  pub fn v82jsc_malloc_size(rt: *mut JSRuntime) -> usize;
  pub fn JS_GetGCThreshold(rt: *mut JSRuntime) -> usize;
  pub fn JS_SetGCThreshold(rt: *mut JSRuntime, gc_threshold: usize);
  pub fn JS_RunGC(rt: *mut JSRuntime);
  pub fn JS_ClearKeptObjects(rt: *mut JSRuntime);
  pub fn v82jsc_new_weak_ref(ctx: *mut JSContext, target: JSValue) -> JSValue;
  pub fn v82jsc_weak_ref_is_live(weak_ref: JSValue) -> bool;
  pub fn v82jsc_weak_ref_target(weak_ref: JSValue) -> JSValue;
  pub fn JS_IsJobPending(rt: *mut JSRuntime) -> bool;
  pub fn JS_ExecutePendingJob(
    rt: *mut JSRuntime,
    pctx: *mut *mut JSContext,
  ) -> c_int;

  // Installs a callback QuickJS polls at safe points (loop back-edges, calls)
  // while JS executes; returning non-zero raises an *uncatchable* "interrupted"
  // InternalError that unwinds the running script. Used to emulate
  // `v8::Isolate::TerminateExecution` for long-running loops.
  pub fn JS_SetInterruptHandler(
    rt: *mut JSRuntime,
    cb: Option<JSInterruptHandler>,
    opaque: *mut c_void,
  );
  pub fn JS_VisitStackFrames(
    rt: *mut JSRuntime,
    visitor: Option<JSStackFrameVisitor>,
    opaque: *mut c_void,
  );
  pub fn JS_SetPreciseCoverageEnabled(ctx: *mut JSContext, enabled: bool);

  // Marks an error value (must be a pending exception) so `try`/`catch` cannot
  // catch it — the same property QuickJS gives its own interrupt exception. Used
  // when we synthesize a termination exception from a native callback boundary.
  pub fn JS_SetUncatchableError(ctx: *mut JSContext, val: JSValue);

  pub fn JS_NewContext(rt: *mut JSRuntime) -> *mut JSContext;
  pub fn JS_NewContextRaw(rt: *mut JSRuntime) -> *mut JSContext;
  pub fn JS_FreeContext(ctx: *mut JSContext);
  pub fn JS_GetRuntime(ctx: *mut JSContext) -> *mut JSRuntime;
  pub fn JS_SetContextOpaque(ctx: *mut JSContext, opaque: *mut c_void);
  pub fn JS_GetContextOpaque(ctx: *mut JSContext) -> *mut c_void;
  pub fn JS_GetGlobalObject(ctx: *mut JSContext) -> JSValue;

  pub fn JS_FreeValue(ctx: *mut JSContext, v: JSValue);
  pub fn JS_FreeValueRT(rt: *mut JSRuntime, v: JSValue);
  pub fn JS_DupValue(ctx: *mut JSContext, v: JSValue) -> JSValue;
  pub fn JS_DupValueRT(rt: *mut JSRuntime, v: JSValue) -> JSValue;

  pub fn JS_NewStringLen(
    ctx: *mut JSContext,
    str: *const c_char,
    len: usize,
  ) -> JSValue;
  pub fn JS_NewAtomString(ctx: *mut JSContext, str: *const c_char) -> JSValue;
  pub fn JS_NewSymbol(
    ctx: *mut JSContext,
    description: *const c_char,
    is_global: c_int,
  ) -> JSValue;
  pub fn JS_NewBigInt64(ctx: *mut JSContext, val: i64) -> JSValue;
  pub fn JS_ToBigInt64(
    ctx: *mut JSContext,
    pres: *mut i64,
    v: JSValue,
  ) -> c_int;
  pub fn JS_NewBigUint64(ctx: *mut JSContext, val: u64) -> JSValue;

  pub fn JS_ToBool(ctx: *mut JSContext, v: JSValue) -> c_int;
  pub fn JS_ToInt32(ctx: *mut JSContext, pres: *mut i32, v: JSValue) -> c_int;
  pub fn JS_ToInt64(ctx: *mut JSContext, pres: *mut i64, v: JSValue) -> c_int;
  pub fn JS_ToFloat64(ctx: *mut JSContext, pres: *mut f64, v: JSValue)
  -> c_int;

  pub fn JS_ToCStringLen2(
    ctx: *mut JSContext,
    plen: *mut usize,
    v: JSValue,
    cesu8: bool,
  ) -> *const c_char;
  pub fn JS_FreeCString(ctx: *mut JSContext, ptr: *const c_char);

  pub fn JS_NewObject(ctx: *mut JSContext) -> JSValue;
  pub fn JS_NewObjectClass(ctx: *mut JSContext, class_id: c_int) -> JSValue;
  pub fn JS_NewArray(ctx: *mut JSContext) -> JSValue;

  pub fn JS_NewArrayBuffer(
    ctx: *mut JSContext,
    buf: *mut u8,
    len: usize,
    free_func: Option<
      unsafe extern "C" fn(*mut JSRuntime, *mut c_void, *mut c_void),
    >,
    opaque: *mut c_void,
    is_shared: bool,
  ) -> JSValue;
  pub fn JS_NewArrayBufferCopy(
    ctx: *mut JSContext,
    buf: *const u8,
    len: usize,
  ) -> JSValue;
  pub fn JS_GetArrayBuffer(
    ctx: *mut JSContext,
    psize: *mut usize,
    obj: JSValue,
  ) -> *mut u8;
  pub fn JS_DetachArrayBuffer(ctx: *mut JSContext, obj: JSValue);

  pub fn JS_GetTypedArrayBuffer(
    ctx: *mut JSContext,
    obj: JSValue,
    pbyte_offset: *mut usize,
    pbyte_length: *mut usize,
    pbytes_per_element: *mut usize,
  ) -> JSValue;

  pub fn JS_IsArray(v: JSValue) -> bool;
  pub fn JS_IsRegExp(v: JSValue) -> bool;
  pub fn JS_IsFunction(ctx: *mut JSContext, v: JSValue) -> bool;
  pub fn JS_IsConstructor(ctx: *mut JSContext, v: JSValue) -> bool;
  pub fn JS_GetPropertyStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
  ) -> JSValue;
  pub fn JS_GetPropertyUint32(
    ctx: *mut JSContext,
    this_obj: JSValue,
    idx: u32,
  ) -> JSValue;
  pub fn JS_SetPropertyStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
  ) -> c_int;
  pub fn JS_SetPropertyUint32(
    ctx: *mut JSContext,
    this_obj: JSValue,
    idx: u32,
    val: JSValue,
  ) -> c_int;

  pub fn JS_HasProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> c_int;
  pub fn JS_DeleteProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    flags: c_int,
  ) -> c_int;
  pub fn JS_Call(
    ctx: *mut JSContext,
    func_obj: JSValue,
    this_obj: JSValue,
    argc: c_int,
    argv: *mut JSValue,
  ) -> JSValue;
  pub fn JS_CallConstructor(
    ctx: *mut JSContext,
    func_obj: JSValue,
    argc: c_int,
    argv: *mut JSValue,
  ) -> JSValue;

  pub fn JS_NewCFunction2(
    ctx: *mut JSContext,
    func: JSCFunction,
    name: *const c_char,
    length: c_int,
    cproto: c_int,
    magic: c_int,
  ) -> JSValue;
  pub fn JS_NewCFunctionData(
    ctx: *mut JSContext,
    func: JSCFunctionData,
    length: c_int,
    magic: c_int,
    data_len: c_int,
    data: *mut JSValue,
  ) -> JSValue;

  pub fn JS_DefinePropertyGetSet(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    getter: JSValue,
    setter: JSValue,
    flags: c_int,
  ) -> c_int;

  pub fn JS_Eval(
    ctx: *mut JSContext,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
    eval_flags: c_int,
  ) -> JSValue;
  pub fn JS_EvalThis(
    ctx: *mut JSContext,
    this_obj: JSValue,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
    eval_flags: c_int,
  ) -> JSValue;
  pub fn JS_EvalFunction(ctx: *mut JSContext, fun_obj: JSValue) -> JSValue;
  pub fn v82jsc_eval_no_side_effect(
    ctx: *mut JSContext,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
  ) -> JSValue;
  pub fn v82jsc_eval_repl(
    ctx: *mut JSContext,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
    eval_flags: c_int,
  ) -> JSValue;
  pub fn v82jsc_live_edit_module(
    ctx: *mut JSContext,
    filename: *const c_char,
    replacement_module: JSValue,
  ) -> c_int;

  pub fn JS_ParseJSON(
    ctx: *mut JSContext,
    buf: *const c_char,
    buf_len: usize,
    filename: *const c_char,
  ) -> JSValue;
  pub fn JS_JSONStringify(
    ctx: *mut JSContext,
    obj: JSValue,
    replacer: JSValue,
    space0: JSValue,
  ) -> JSValue;

  pub fn JS_WriteObject(
    ctx: *mut JSContext,
    psize: *mut usize,
    obj: JSValue,
    flags: c_int,
  ) -> *mut u8;
  pub fn JS_ReadObject(
    ctx: *mut JSContext,
    buf: *const u8,
    buf_len: usize,
    flags: c_int,
  ) -> JSValue;

  pub fn JS_NewPromiseCapability(
    ctx: *mut JSContext,
    resolving_funcs: *mut JSValue,
  ) -> JSValue;
  pub fn JS_PromiseThen(
    ctx: *mut JSContext,
    promise: JSValue,
    on_fulfilled: JSValue,
    on_rejected: JSValue,
  ) -> JSValue;
  pub fn JS_PromiseState(ctx: *mut JSContext, promise: JSValue) -> c_int;
  pub fn JS_PromiseResult(ctx: *mut JSContext, promise: JSValue) -> JSValue;
  pub fn JS_IsPromise(v: JSValue) -> bool;
  pub fn JS_PromiseMarkAsHandled(ctx: *mut JSContext, promise: JSValue);
  pub fn JS_SetHostPromiseRejectionTracker(
    rt: *mut JSRuntime,
    cb: Option<JSHostPromiseRejectionTracker>,
    opaque: *mut c_void,
  );

  pub fn JS_SetModuleLoaderFunc(
    rt: *mut JSRuntime,
    normalize: Option<JSModuleNormalizeFunc>,
    loader: Option<JSModuleLoaderFunc>,
    opaque: *mut c_void,
  );
  pub fn JS_SetModuleLoaderFunc2(
    rt: *mut JSRuntime,
    normalize: Option<JSModuleNormalizeFunc>,
    loader: Option<JSModuleLoaderFunc2>,
    check_attributes: Option<JSModuleCheckSupportedImportAttributes>,
    opaque: *mut c_void,
  );
  pub fn JS_GetModuleName(ctx: *mut JSContext, m: *mut JSModuleDef) -> JSAtom;

  pub fn JS_GetImportMeta(ctx: *mut JSContext, m: *mut JSModuleDef) -> JSValue;

  pub fn v82jsc_module_is_evaluated(m: *mut JSModuleDef)
  -> std::os::raw::c_int;
  pub fn v82jsc_module_get_exception(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
  ) -> JSValue;
  pub fn v82jsc_module_eval_started(m: *mut JSModuleDef)
  -> std::os::raw::c_int;
  pub fn v82jsc_module_is_evaluating_async(
    m: *mut JSModuleDef,
  ) -> std::os::raw::c_int;
  pub fn v82jsc_has_loaded_module(
    ctx: *mut JSContext,
    name: *const std::os::raw::c_char,
  ) -> std::os::raw::c_int;
  pub fn v82jsc_get_loaded_module(
    ctx: *mut JSContext,
    name: *const std::os::raw::c_char,
  ) -> *mut JSModuleDef;

  pub fn JS_SetDynamicImportHook(fn_: JSV82jscDynImportHook);
  pub fn JS_SetImportMetaHook(fn_: Option<JSV82jscImportMetaHook>);
  pub fn v82jsc_mark_import_meta_initialized(module: *mut JSModuleDef);
  pub fn JS_GetModuleNamespace(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
  ) -> JSValue;
  pub fn v82jsc_new_module_namespace(ctx: *mut JSContext) -> JSValue;
  pub fn v82jsc_module_namespace_set(
    ctx: *mut JSContext,
    namespace: JSValue,
    name: *const c_char,
    value: JSValue,
  ) -> c_int;
  pub fn v82jsc_global_var_obj(ctx: *mut JSContext) -> JSValue;
  pub fn v82jsc_heap_objects(
    ctx: *mut JSContext,
    count: *mut usize,
  ) -> *mut JSValue;
  pub fn v82jsc_free_heap_objects(
    ctx: *mut JSContext,
    objects: *mut JSValue,
    count: usize,
  );
  pub fn JS_NewCModule(
    ctx: *mut JSContext,
    name_str: *const c_char,
    func: Option<JSModuleInitFunc>,
  ) -> *mut JSModuleDef;
  pub fn JS_GetPrototype(ctx: *mut JSContext, val: JSValue) -> JSValue;
  pub fn JS_AddModuleExport(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
    name_str: *const c_char,
  ) -> c_int;
  pub fn JS_SetModuleExport(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
    export_name: *const c_char,
    val: JSValue,
  ) -> c_int;

  pub fn js_v82jsc_function_kind(v: JSValue) -> std::os::raw::c_int;
  pub fn v82jsc_adjust_function_line_number(
    func_obj: JSValue,
    line_delta: std::os::raw::c_int,
  );

  pub fn js_v82jsc_iterator_preview(
    ctx: *mut JSContext,
    iter: JSValue,
    pis_key_value: *mut std::os::raw::c_int,
  ) -> JSValue;

  pub fn JS_IsProxy(val: JSValue) -> bool;
  pub fn JS_GetProxyTarget(ctx: *mut JSContext, proxy: JSValue) -> JSValue;
  pub fn JS_GetProxyHandler(ctx: *mut JSContext, proxy: JSValue) -> JSValue;
  pub fn JS_NewProxy(
    ctx: *mut JSContext,
    target: JSValue,
    handler: JSValue,
  ) -> JSValue;

  pub fn JS_Throw(ctx: *mut JSContext, obj: JSValue) -> JSValue;
  pub fn JS_GetException(ctx: *mut JSContext) -> JSValue;
  // QuickJS declares this `bool` (1 byte). Binding it as `c_int` is a latent
  // ABI bug: on x86_64 a `bool` return only defines the low byte of the return
  // register (the upper bytes are garbage), so `JS_HasException(ctx) != 0`
  // reads true even when the real bool is false — which spuriously trips
  // `TryCatch::HasCaught` after a JS `try/catch` swallows a native-callback
  // exception. (aarch64 zero-extends the byte, so the bug is invisible on the
  // macOS worker.) Must match the C signature exactly.
  pub fn JS_HasException(ctx: *mut JSContext) -> bool;
  pub fn JS_SetPrepareStackTraceCallback(
    ctx: *mut JSContext,
    callback: JSValue,
  );
  pub fn JS_GetPrepareStackTraceCallback(ctx: *mut JSContext) -> JSValue;
  pub fn JS_ResetUncatchableError(ctx: *mut JSContext);
  pub fn JS_ThrowTypeError(
    ctx: *mut JSContext,
    fmt: *const c_char,
    ...
  ) -> JSValue;
  pub fn JS_ThrowReferenceError(
    ctx: *mut JSContext,
    fmt: *const c_char,
    ...
  ) -> JSValue;
  pub fn JS_ThrowSyntaxError(
    ctx: *mut JSContext,
    fmt: *const c_char,
    ...
  ) -> JSValue;
  pub fn JS_ThrowRangeError(
    ctx: *mut JSContext,
    fmt: *const c_char,
    ...
  ) -> JSValue;
  pub fn JS_ThrowInternalError(
    ctx: *mut JSContext,
    fmt: *const c_char,
    ...
  ) -> JSValue;
  pub fn JS_ThrowOutOfMemory(ctx: *mut JSContext) -> JSValue;
  pub fn v82jsc_is_constructor_call(ctx: *mut JSContext) -> bool;

  pub fn JS_NewAtom(ctx: *mut JSContext, str: *const c_char) -> JSAtom;

  pub fn js_malloc(ctx: *mut JSContext, size: usize) -> *mut c_void;
  pub fn js_free(ctx: *mut JSContext, ptr: *mut c_void);
  pub fn js_strdup(ctx: *mut JSContext, s: *const c_char) -> *mut c_char;

  pub fn JS_NewAtomLen(
    ctx: *mut JSContext,
    str: *const c_char,
    len: usize,
  ) -> JSAtom;
  pub fn JS_FreeAtom(ctx: *mut JSContext, v: JSAtom);
  pub fn JS_AtomToString(ctx: *mut JSContext, atom: JSAtom) -> JSValue;
  pub fn JS_AtomToValue(ctx: *mut JSContext, atom: JSAtom) -> JSValue;
}

pub const JS_CFUNC_GENERIC: c_int = 0;
pub const JS_CFUNC_GENERIC_MAGIC: c_int = 1;
pub const JS_CFUNC_CONSTRUCTOR: c_int = 2;
pub const JS_CFUNC_CONSTRUCTOR_OR_FUNC: c_int = 4;
pub const JS_CFUNC_CONSTRUCTOR_OR_FUNC_MAGIC_RECEIVER: c_int = 13;

#[inline]
pub unsafe fn JS_NewCFunction(
  ctx: *mut JSContext,
  func: JSCFunction,
  name: *const c_char,
  length: c_int,
) -> JSValue {
  unsafe { JS_NewCFunction2(ctx, func, name, length, JS_CFUNC_GENERIC, 0) }
}

#[inline]
pub unsafe fn JS_ToCString(ctx: *mut JSContext, v: JSValue) -> *const c_char {
  unsafe { JS_ToCStringLen2(ctx, std::ptr::null_mut(), v, false) }
}

#[inline]
pub unsafe fn JS_ToCStringLen(
  ctx: *mut JSContext,
  plen: *mut usize,
  v: JSValue,
) -> *const c_char {
  unsafe { JS_ToCStringLen2(ctx, plen, v, false) }
}

#[inline]
pub unsafe fn JS_HasPropertyStr(
  ctx: *mut JSContext,
  this_obj: JSValue,
  prop: *const c_char,
) -> c_int {
  unsafe {
    let atom = JS_NewAtom(ctx, prop);
    let r = JS_HasProperty(ctx, this_obj, atom);
    JS_FreeAtom(ctx, atom);
    r
  }
}

#[inline]
pub unsafe fn JS_DeletePropertyStr(
  ctx: *mut JSContext,
  this_obj: JSValue,
  prop: *const c_char,
  flags: c_int,
) -> c_int {
  unsafe {
    let atom = JS_NewAtom(ctx, prop);
    let r = JS_DeleteProperty(ctx, this_obj, atom, flags);
    JS_FreeAtom(ctx, atom);
    r
  }
}

#[inline]
pub unsafe fn JS_NewBool(_ctx: *mut JSContext, val: c_int) -> JSValue {
  jsv_bool(val != 0)
}
#[inline]
pub unsafe fn JS_NewInt32(_ctx: *mut JSContext, val: i32) -> JSValue {
  jsv_int32(val)
}
#[inline]
pub unsafe fn JS_NewUint32(_ctx: *mut JSContext, val: u32) -> JSValue {
  if val <= i32::MAX as u32 {
    jsv_int32(val as i32)
  } else {
    jsv_float64(val as f64)
  }
}
#[inline]
pub unsafe fn JS_NewInt64(_ctx: *mut JSContext, val: i64) -> JSValue {
  if val >= i32::MIN as i64 && val <= i32::MAX as i64 {
    jsv_int32(val as i32)
  } else {
    jsv_float64(val as f64)
  }
}
#[inline]
pub unsafe fn JS_NewFloat64(_ctx: *mut JSContext, val: f64) -> JSValue {
  jsv_float64(val)
}
#[inline]
pub unsafe fn JS_NewString(ctx: *mut JSContext, s: *const c_char) -> JSValue {
  let len = if s.is_null() {
    0
  } else {
    unsafe { libc_strlen(s) }
  };
  unsafe { JS_NewStringLen(ctx, s, len) }
}
unsafe extern "C" {
  #[link_name = "strlen"]
  fn libc_strlen(s: *const c_char) -> usize;
}
