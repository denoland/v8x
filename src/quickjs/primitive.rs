#![allow(non_snake_case)]

use crate::quickjs::core::{
  JS_TAG_V8_CONTEXT, ctx_of, current_ctx, intern, iso_state, jsval_of,
};
use crate::quickjs::function::{TemplKind, template_kind};
use crate::quickjs::quickjs_sys::*;
use crate::support::int;
use crate::{
  BigInt, Boolean, Context, Data, Int32, Integer, Number, Primitive, Private,
  RealIsolate, String as V8String, Symbol, Uint32, Value,
};
use std::ffi::CString;
use std::ptr;

unsafe extern "C" {
  fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
  fn JS_ToBigInt64(
    ctx: *mut JSContext,
    pres: *mut i64,
    val: JSValue,
  ) -> std::os::raw::c_int;
  fn JS_ToBigUint64(
    ctx: *mut JSContext,
    pres: *mut u64,
    val: JSValue,
  ) -> std::os::raw::c_int;
}

#[inline]
fn iso_ctx(isolate: *mut RealIsolate) -> *mut JSContext {
  if isolate.is_null() {
    return current_ctx();
  }
  let st = iso_state(isolate);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

unsafe fn eval(ctx: *mut JSContext, src: &str) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }
  let Ok(c) = CString::new(src) else {
    return jsv_undefined();
  };
  let fname = c"<v82jsc>";
  let v = JS_Eval(
    ctx,
    c.as_ptr(),
    src.len(),
    fname.as_ptr(),
    JS_EVAL_TYPE_GLOBAL,
  );
  if v.tag == JS_TAG_EXCEPTION {
    let exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    return jsv_undefined();
  }
  v
}

unsafe fn to_dec_string(
  ctx: *mut JSContext,
  v: JSValue,
) -> std::string::String {
  let mut len: usize = 0;
  let s = JS_ToCStringLen(ctx, &mut len, v);
  if s.is_null() {
    let exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    return std::string::String::new();
  }
  let bytes = std::slice::from_raw_parts(s as *const u8, len);
  let out = std::string::String::from_utf8_lossy(bytes).into_owned();
  JS_FreeCString(ctx, s);
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(
  isolate: *mut RealIsolate,
  value: f64,
) -> *const Number {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewFloat64(ctx, value) };
  let h = intern::<Number>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__Value(this: *const Number) -> f64 {
  let ctx = current_ctx();
  let v = jsval_of(this);
  if ctx.is_null() {
    return match v.tag {
      JS_TAG_INT => unsafe { v.u.int32 as f64 },
      JS_TAG_FLOAT64 => unsafe { v.u.float64 },
      _ => f64::NAN,
    };
  }
  let mut out: f64 = f64::NAN;
  unsafe { JS_ToFloat64(ctx, &mut out, v) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__New(
  isolate: *mut RealIsolate,
  value: i32,
) -> *const Integer {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewInt32(ctx, value) };
  let h = intern::<Integer>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__NewFromUnsigned(
  isolate: *mut RealIsolate,
  value: u32,
) -> *const Integer {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let v = unsafe { JS_NewUint32(ctx, value) };
  let h = intern::<Integer>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__Value(this: *const Integer) -> i64 {
  let v = jsval_of(this);
  match v.tag {
    JS_TAG_INT => unsafe { v.u.int32 as i64 },
    JS_TAG_FLOAT64 => unsafe { v.u.float64 as i64 },
    _ => {
      let ctx = current_ctx();
      if ctx.is_null() {
        return 0;
      }
      let mut out: i64 = 0;
      unsafe { JS_ToInt64(ctx, &mut out, v) };
      out
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Int32__Value(this: *const Int32) -> i32 {
  let v = jsval_of(this);
  if v.tag == JS_TAG_INT {
    return unsafe { v.u.int32 };
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return unsafe {
      match v.tag {
        JS_TAG_FLOAT64 => v.u.float64 as i64 as i32,
        _ => 0,
      }
    };
  }
  let mut out: i32 = 0;
  unsafe { JS_ToInt32(ctx, &mut out, v) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Uint32__Value(this: *const Uint32) -> u32 {
  let v = jsval_of(this);
  if v.tag == JS_TAG_INT {
    return unsafe { v.u.int32 as u32 };
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return unsafe {
      match v.tag {
        JS_TAG_FLOAT64 => v.u.float64 as i64 as u32,
        _ => 0,
      }
    };
  }

  let mut out: i32 = 0;
  unsafe { JS_ToInt32(ctx, &mut out, v) };
  out as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(
  isolate: *mut RealIsolate,
  value: bool,
) -> *const Boolean {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewBool(ctx, value as std::os::raw::c_int) };
  let h = intern::<Boolean>(v);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(isolate: *mut RealIsolate) -> *const Primitive {
  let _ = isolate;

  let h = intern::<Primitive>(jsv_null());
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__New(
  isolate: *mut RealIsolate,
  value: i64,
) -> *const BigInt {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewBigInt64(ctx, value) };
  intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromUnsigned(
  isolate: *mut RealIsolate,
  value: u64,
) -> *const BigInt {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_NewBigUint64(ctx, value) };
  intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromWords(
  context: *const Context,
  sign_bit: int,
  word_count: int,
  words: *const u64,
) -> *const BigInt {
  let ctx = ctx_of(context);
  if ctx.is_null() || (word_count > 0 && words.is_null()) {
    return ptr::null();
  }

  let n = word_count.max(0) as usize;
  let mut expr = std::string::String::from("(");
  for i in 0..n {
    let w = unsafe { *words.add(i) };
    if i > 0 {
      expr.push('+');
    }
    expr.push_str(&format!("(BigInt(\"{w}\")<<{}n)", 64u64 * i as u64));
  }
  if n == 0 {
    expr.push_str("0n");
  }
  expr.push(')');
  if sign_bit != 0 {
    expr = format!("(-{expr})");
  }
  let v = unsafe { eval(ctx, &expr) };
  intern::<BigInt>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Uint64Value(
  this: *const BigInt,
  lossless: *mut bool,
) -> u64 {
  let ctx = current_ctx();
  let val = jsval_of(this);
  if ctx.is_null() {
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }
  let mut out: u64 = 0;

  let rc = unsafe { JS_ToBigUint64(ctx, &mut out, val) };
  if rc != 0 {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }
  if !lossless.is_null() {
    unsafe {
      *lossless = bigint_eq_self(ctx, val, "BigInt.asUintN(64,__v)===__v")
    };
  }
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Int64Value(
  this: *const BigInt,
  lossless: *mut bool,
) -> i64 {
  let ctx = current_ctx();
  let val = jsval_of(this);
  if ctx.is_null() {
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }
  let mut out: i64 = 0;
  let rc = unsafe { JS_ToBigInt64(ctx, &mut out, val) };
  if rc != 0 {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }
  if !lossless.is_null() {
    unsafe {
      *lossless = bigint_eq_self(ctx, val, "BigInt.asIntN(64,__v)===__v")
    };
  }
  out
}

unsafe fn bigint_eq_self(
  ctx: *mut JSContext,
  val: JSValue,
  pred: &str,
) -> bool {
  let dec = to_dec_string(ctx, val);
  if dec.is_empty() {
    return false;
  }
  let src = format!("((__v)=>({pred}))(BigInt(\"{dec}\"))");
  let r = eval(ctx, &src);
  let b = JS_ToBool(ctx, r) != 0;
  JS_FreeValue(ctx, r);
  b
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__WordCount(this: *const BigInt) -> int {
  let ctx = current_ctx();
  let val = jsval_of(this);
  if ctx.is_null() {
    return 0;
  }
  let dec = unsafe { to_dec_string(ctx, val) };
  if dec.is_empty() {
    return 0;
  }
  let src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
  );
  let r = unsafe { eval(ctx, &src) };
  let mut out: i32 = 0;
  unsafe { JS_ToInt32(ctx, &mut out, r) };
  unsafe { JS_FreeValue(ctx, r) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__ToWordsArray(
  this: *const BigInt,
  sign_bit: *mut int,
  word_count: *mut int,
  words: *mut u64,
) {
  let ctx = current_ctx();
  let val = jsval_of(this);
  let avail = if word_count.is_null() {
    0
  } else {
    unsafe { *word_count }.max(0) as usize
  };
  if ctx.is_null() {
    if !sign_bit.is_null() {
      unsafe { *sign_bit = 0 };
    }
    if !word_count.is_null() {
      unsafe { *word_count = 0 };
    }
    return;
  }
  let dec = unsafe { to_dec_string(ctx, val) };
  let neg = dec.starts_with('-');
  if !sign_bit.is_null() {
    unsafe { *sign_bit = if neg { 1 } else { 0 } };
  }

  let total_src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
  );
  let total = unsafe {
    let r = eval(ctx, &total_src);
    let mut t: i32 = 0;
    JS_ToInt32(ctx, &mut t, r);
    JS_FreeValue(ctx, r);
    t.max(0) as usize
  };
  let to_write = total.min(avail);
  for i in 0..to_write {
    let w = unsafe { bigint_word_at(ctx, &dec, i) };
    unsafe { *words.add(i) = w };
  }
  if !word_count.is_null() {
    unsafe { *word_count = total as int };
  }
}

unsafe fn bigint_word_at(ctx: *mut JSContext, dec: &str, i: usize) -> u64 {
  let shift = 64u64 * i as u64;
  let src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return BigInt.asUintN(64,x>>{shift}n);}})()"
  );
  let r = eval(ctx, &src);
  if r.tag == JS_TAG_EXCEPTION {
    JS_FreeValue(ctx, r);
    return 0;
  }
  let mut word = 0;
  let rc = JS_ToBigUint64(ctx, &mut word, r);
  JS_FreeValue(ctx, r);
  if rc == 0 {
    word
  } else {
    let exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    0
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__ForApi(
  isolate: *mut RealIsolate,
  name: *const V8String,
) -> *const Private {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if name.is_null() {
    std::string::String::new()
  } else {
    unsafe { to_dec_string(ctx, jsval_of(name)) }
  };
  let name_value = if name.is_null() {
    jsv_undefined()
  } else {
    jsval_of(name)
  };
  let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
  let v =
    unsafe { eval(ctx, &format!("Symbol.for(\"v82jsc_private:{escaped}\")")) };
  remember_private_symbol(isolate, ctx, v, name_value);
  intern::<Private>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__New(
  isolate: *mut RealIsolate,
  name: *const V8String,
) -> *const Private {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if name.is_null() {
    std::string::String::new()
  } else {
    unsafe { to_dec_string(ctx, jsval_of(name)) }
  };
  let name_value = if name.is_null() {
    jsv_undefined()
  } else {
    jsval_of(name)
  };
  let Ok(cdesc) = CString::new(desc) else {
    return ptr::null();
  };

  let v = unsafe { JS_NewSymbol(ctx, cdesc.as_ptr(), 0) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  remember_private_symbol(isolate, ctx, v, name_value);
  intern::<Private>(v)
}

fn remember_private_symbol(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  value: JSValue,
  name: JSValue,
) {
  let iso = if isolate.is_null() {
    crate::quickjs::core::current_iso()
  } else {
    isolate
  };
  if iso.is_null() || ctx.is_null() || value.tag != JS_TAG_SYMBOL {
    return;
  }
  let stored = unsafe { JS_DupValue(ctx, value) };
  let stored_name = unsafe { JS_DupValue(ctx, name) };
  iso_state(iso).private_symbols.push((stored, stored_name));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__For(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if description.is_null() {
    std::string::String::new()
  } else {
    unsafe { to_dec_string(ctx, jsval_of(description)) }
  };
  let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
  let v = unsafe { eval(ctx, &format!("Symbol.for(\"{escaped}\")")) };
  intern::<Symbol>(v)
}

thread_local! {
  // Embedder "API" symbol registry (V8's `Symbol::ForApi`). It is a registry
  // *distinct* from `Symbol.for` (so `for_api(d) != for_key(d)`) but idempotent
  // (`for_api(d) == for_api(d)`), with the symbol's description preserved. We
  // back it with our own per-context cache of fresh, unique symbols.
  static API_SYMBOLS: std::cell::RefCell<
    std::collections::HashMap<(usize, std::string::String), JSValue>,
  > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__ForApi(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if description.is_null() {
    std::string::String::new()
  } else {
    unsafe { to_dec_string(ctx, jsval_of(description)) }
  };

  API_SYMBOLS.with(|m| {
    let key = (ctx as usize, desc.clone());
    if let Some(v) = m.borrow().get(&key) {
      let dup = unsafe { JS_DupValue(ctx, *v) };
      return intern::<Symbol>(dup);
    }
    let Ok(cdesc) = CString::new(desc) else {
      return ptr::null();
    };
    // Fresh, unique (non-global) symbol — never equal to a `Symbol.for` result.
    let v = unsafe { JS_NewSymbol(ctx, cdesc.as_ptr(), 0) };
    if v.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      return ptr::null();
    }
    // Keep a duplicate alive in the registry so future lookups return the same
    // symbol identity.
    let stored = unsafe { JS_DupValue(ctx, v) };
    m.borrow_mut().insert(key, stored);
    intern::<Symbol>(v)
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__New(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let cdesc = if description.is_null() {
    None
  } else {
    let desc = unsafe { to_dec_string(ctx, jsval_of(description)) };
    let Ok(cdesc) = CString::new(desc) else {
      return ptr::null();
    };
    Some(cdesc)
  };

  let cdesc_ptr = cdesc.as_ref().map_or(ptr::null(), |c| c.as_ptr());
  let v = unsafe { JS_NewSymbol(ctx, cdesc_ptr, 0) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Symbol>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__Description(
  this: *const Symbol,
  isolate: *mut RealIsolate,
) -> *const Value {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  unsafe {
    let desc = symbol_description(ctx, jsval_of(this));
    if desc.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<Value>(desc)
  }
}

unsafe fn symbol_description(ctx: *mut JSContext, sym: JSValue) -> JSValue {
  const SRC: &[u8] = b"this.description\0";
  unsafe {
    JS_EvalThis(
      ctx,
      sym,
      SRC.as_ptr() as *const std::os::raw::c_char,
      SRC.len() - 1,
      c"<sym-desc>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  }
}

unsafe fn well_known(ctx: *mut JSContext, prop: &[u8]) -> JSValue {
  if ctx.is_null() {
    return jsv_undefined();
  }
  let global = JS_GetGlobalObject(ctx);
  let sym_ctor = JS_GetPropertyStr(ctx, global, c"Symbol".as_ptr());
  JS_FreeValue(ctx, global);
  if sym_ctor.tag == JS_TAG_EXCEPTION {
    let exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    return jsv_undefined();
  }

  let v = JS_GetPropertyStr(
    ctx,
    sym_ctor,
    prop.as_ptr() as *const std::os::raw::c_char,
  );
  JS_FreeValue(ctx, sym_ctor);
  if v.tag == JS_TAG_EXCEPTION {
    let exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    return jsv_undefined();
  }
  v
}

macro_rules! well_known_symbol {
  ($fn_name:ident, $prop:expr) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(isolate: *mut RealIsolate) -> *const Symbol {
      let ctx = iso_ctx(isolate);
      if ctx.is_null() {
        return ptr::null();
      }
      let v = unsafe { well_known(ctx, $prop) };
      intern::<Symbol>(v)
    }
  };
}

well_known_symbol!(v8__Symbol__GetAsyncIterator, b"asyncIterator\0");
well_known_symbol!(v8__Symbol__GetHasInstance, b"hasInstance\0");
well_known_symbol!(v8__Symbol__GetIsConcatSpreadable, b"isConcatSpreadable\0");
well_known_symbol!(v8__Symbol__GetIterator, b"iterator\0");
well_known_symbol!(v8__Symbol__GetMatch, b"match\0");
well_known_symbol!(v8__Symbol__GetReplace, b"replace\0");
well_known_symbol!(v8__Symbol__GetSearch, b"search\0");
well_known_symbol!(v8__Symbol__GetSplit, b"split\0");
well_known_symbol!(v8__Symbol__GetToPrimitive, b"toPrimitive\0");
well_known_symbol!(v8__Symbol__GetToStringTag, b"toStringTag\0");
well_known_symbol!(v8__Symbol__GetUnscopables, b"unscopables\0");
well_known_symbol!(v8__Symbol__GetDispose, b"dispose\0");
well_known_symbol!(v8__Symbol__GetAsyncDispose, b"asyncDispose\0");

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__EQ(this: *const Data, other: *const Data) -> bool {
  // Two handles to one slot always name the same thing. This also covers
  // templates, whose handles are not JSValue slots at all.
  if ptr::eq(this, other) {
    return !this.is_null();
  }
  let a = jsval_of(this);
  let b = jsval_of(other);

  if a.tag == JS_TAG_V8_CONTEXT || b.tag == JS_TAG_V8_CONTEXT {
    return a.tag == b.tag && unsafe { a.u.ptr == b.u.ptr };
  }
  if a.tag == b.tag && a.tag < 0 && unsafe { a.u.ptr == b.u.ptr } {
    return true;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return a.tag == b.tag && unsafe { a.u.ptr == b.u.ptr };
  }
  unsafe { JS_IsStrictEqual(ctx, a, b) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsValue(this: *const Data) -> bool {
  !this.is_null()
    && template_kind(this as *const _).is_none()
    && !v8__Data__IsModule(this)
    && !v8__Data__IsModuleRequest(this)
    && !v8__Data__IsPrivate(this as *const _)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrimitive(this: *const Data) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);

  v.tag != JS_TAG_OBJECT
    && v.tag != JS_TAG_MODULE
    && v.tag != JS_TAG_FUNCTION_BYTECODE
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFunctionTemplate(this: *const Data) -> bool {
  template_kind(this as *const _) == Some(TemplKind::Func)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsDictionaryTemplate(this: *const Data) -> bool {
  template_kind(this as *const _) == Some(TemplKind::Dict)
}

/// Whether `this` can be used while scopes of `isolate` are open. Contexts
/// record the runtime they belong to, so a context of another isolate is
/// caught. Other values carry no owner that is cheap to read (a QuickJS object
/// does not point back at its runtime), and this runs on every `Local`
/// dereference in debug builds, so they are assumed to belong.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsCompatibleWithIsolate(
  this: *const Data,
  isolate: *const RealIsolate,
) -> bool {
  if this.is_null() || isolate.is_null() {
    return true;
  }
  let v = jsval_of(this);
  if v.tag != JS_TAG_V8_CONTEXT || template_kind(this as *const _).is_some() {
    return true;
  }
  let ctx = unsafe { v.u.ptr } as *mut JSContext;
  let rt = iso_state(isolate as *mut RealIsolate).rt;
  ctx.is_null() || rt.is_null() || unsafe { JS_GetRuntime(ctx) } == rt
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__Value(this: *const Boolean) -> bool {
  if this.is_null() {
    return false;
  }
  let v = jsval_of(this);
  if v.tag == JS_TAG_BOOL {
    return unsafe { v.u.int32 } != 0;
  }
  let ctx = current_ctx();
  !ctx.is_null() && unsafe { JS_ToBool(ctx, v) } > 0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModule(this: *const Data) -> bool {
  if this.is_null() {
    return false;
  }
  // Module WRAPPER handles are plain objects tag-wise; identity lives in the
  // module side tables (snapshot restores hand these back through
  // GetDataFromSnapshotOnce and rusty_v8 type-checks them).
  let r = jsval_of(this).tag == JS_TAG_MODULE
    || crate::quickjs::module::is_module_wrapper(this as *const _);
  if !r && std::env::var_os("QJS_DEBUG_TAPE").is_some() {
    eprintln!(
      "[qjs tape] IsModule=false ptr={this:?} tag={}",
      jsval_of(this).tag
    );
  }
  r
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModuleRequest(this: *const Data) -> bool {
  has_marker(this, c"__v8jsc_module_request")
}

fn has_marker(this: *const Data, key: &std::ffi::CStr) -> bool {
  if this.is_null() {
    return false;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return false;
  }
  let v = jsval_of(this);
  if v.tag != JS_TAG_OBJECT {
    return false;
  }
  unsafe {
    let prop = JS_GetPropertyStr(ctx, v, key.as_ptr());
    if prop.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return false;
    }
    let b = JS_ToBool(ctx, prop) != 0;
    JS_FreeValue(ctx, prop);
    b
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
pub extern "C" fn v8__Data__IsObjectTemplate(
  this: *const std::os::raw::c_void,
) -> bool {
  template_kind(this) == Some(TemplKind::Obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrivate(
  this: *const std::os::raw::c_void,
) -> bool {
  if this.is_null() {
    return false;
  }
  let ctx = current_ctx();
  let iso = crate::quickjs::core::current_iso();
  if ctx.is_null() || iso.is_null() {
    return false;
  }
  let value = jsval_of(this as *const Data);
  if value.tag != JS_TAG_SYMBOL {
    return false;
  }
  iso_state(iso)
    .private_symbols
    .iter()
    .any(|&(private, _)| unsafe { JS_IsStrictEqual(ctx, value, private) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__Name(this: *const Private) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let ctx = current_ctx();
  let iso = crate::quickjs::core::current_iso();
  if ctx.is_null() || iso.is_null() {
    return ptr::null();
  }
  let value = jsval_of(this);
  let Some((_, name)) = iso_state(iso)
    .private_symbols
    .iter()
    .find(|&&(private, _)| unsafe { JS_IsStrictEqual(ctx, value, private) })
  else {
    return ptr::null();
  };
  intern::<Value>(unsafe { JS_DupValue(ctx, *name) })
}
