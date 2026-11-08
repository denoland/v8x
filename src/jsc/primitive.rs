#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::support::int;
use crate::{
  BigInt, Boolean, Context, Data, Int32, Integer, Number, Primitive, Private,
  RealIsolate, String as V8String, Symbol, Value,
};
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr;

#[inline]
unsafe fn eval(ctx: JSContextRef, src: &str) -> JSValueRef {
  if ctx.is_null() {
    return ptr::null();
  }
  let Ok(c) = CString::new(src) else {
    return ptr::null();
  };
  let s = JSStringCreateWithUTF8CString(c.as_ptr());
  if s.is_null() {
    return ptr::null();
  }
  let mut exc: JSValueRef = ptr::null();
  let v =
    JSEvaluateScript(ctx, s, ptr::null_mut(), ptr::null_mut(), 0, &mut exc);
  JSStringRelease(s);
  if !exc.is_null() {
    return ptr::null();
  }
  v
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(
  isolate: *mut RealIsolate,
  value: f64,
) -> *const Number {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeNumber(ctx, value) };
  intern_ctx::<Number>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__Value(this: *const Number) -> f64 {
  let ctx = current_ctx();
  if ctx.is_null() {
    return 0.0;
  }
  let mut exc: JSValueRef = ptr::null();
  unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__New(
  isolate: *mut RealIsolate,
  value: i32,
) -> *const Integer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeNumber(ctx, value as f64) };
  intern_ctx::<Integer>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__NewFromUnsigned(
  isolate: *mut RealIsolate,
  value: u32,
) -> *const Integer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeNumber(ctx, value as f64) };
  intern_ctx::<Integer>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__Value(this: *const Integer) -> i64 {
  let ctx = current_ctx();
  if ctx.is_null() {
    return 0;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
  n as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Int32__Value(this: *const Int32) -> i32 {
  let ctx = current_ctx();
  if ctx.is_null() {
    return 0;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
  n as i64 as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Uint32__Value(this: *const crate::Uint32) -> u32 {
  let ctx = current_ctx();
  if ctx.is_null() {
    return 0;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(ctx, jsval(this), &mut exc) };
  n as i64 as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(
  isolate: *mut RealIsolate,
  value: bool,
) -> *const Boolean {
  // V8 creates booleans without an entered context.
  let ctx = crate::jsc::core::ensure_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeBoolean(ctx, value) };
  intern_ctx::<Boolean>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__Value(this: *const Boolean) -> bool {
  if this.is_null() {
    return false;
  }
  let ctx = crate::jsc::core::ensure_ctx(current_iso());
  if ctx.is_null() {
    return false;
  }
  unsafe { JSValueToBoolean(ctx, jsval(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(isolate: *mut RealIsolate) -> *const Primitive {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeNull(ctx) };
  intern_ctx::<Primitive>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__New(
  isolate: *mut RealIsolate,
  value: i64,
) -> *const BigInt {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let v = unsafe { eval(ctx, &format!("BigInt(\"{value}\")")) };
  intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromUnsigned(
  isolate: *mut RealIsolate,
  value: u64,
) -> *const BigInt {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let v = unsafe { eval(ctx, &format!("BigInt(\"{value}\")")) };
  intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__NewFromWords(
  context: *const Context,
  sign_bit: int,
  word_count: int,
  words: *const u64,
) -> *const BigInt {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || (word_count > 0 && words.is_null()) {
    return ptr::null();
  }

  let mut expr = std::string::String::from("(");
  for i in 0..word_count.max(0) as usize {
    let w = unsafe { *words.add(i) };
    if i > 0 {
      expr.push('+');
    }

    expr.push_str(&format!("(BigInt(\"{w}\")<<{}n)", 64u64 * i as u64));
  }
  if word_count <= 0 {
    expr.push_str("0n");
  }
  expr.push(')');
  if sign_bit != 0 {
    expr = format!("(-{expr})");
  }
  let v = unsafe { eval(ctx, &expr) };
  intern_ctx::<BigInt>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Uint64Value(
  this: *const BigInt,
  lossless: *mut bool,
) -> u64 {
  let ctx = current_ctx();
  let val = jsval(this);
  if ctx.is_null() || val.is_null() {
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }

  unsafe {
    let truncated = bigint_to_u64(ctx, val);
    if !lossless.is_null() {
      let chk = format!("((__v)=>(__v>=0n && __v===BigInt.asUintN(64,__v)))",);
      *lossless = bigint_predicate(ctx, val, &chk);
    }
    truncated
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__Int64Value(
  this: *const BigInt,
  lossless: *mut bool,
) -> i64 {
  let ctx = current_ctx();
  let val = jsval(this);
  if ctx.is_null() || val.is_null() {
    if !lossless.is_null() {
      unsafe { *lossless = false };
    }
    return 0;
  }
  unsafe {
    let truncated = bigint_to_u64(ctx, val) as i64;
    if !lossless.is_null() {
      let chk = "((__v)=>(__v===BigInt.asIntN(64,__v)))";
      *lossless = bigint_predicate(ctx, val, chk);
    }
    truncated
  }
}

unsafe fn bigint_predicate(
  ctx: JSContextRef,
  val: JSValueRef,
  func_src: &str,
) -> bool {
  let stash = "globalThis.__v82jsc_bi";

  if !stash_value(ctx, stash, val) {
    return false;
  }
  let src = format!("({func_src})({stash})");
  let r = eval(ctx, &src);
  if r.is_null() {
    return false;
  }
  JSValueToBoolean(ctx, r)
}

unsafe fn stash_value(ctx: JSContextRef, path: &str, val: JSValueRef) -> bool {
  let mut exc: JSValueRef = ptr::null();
  let s = JSValueToStringCopy(ctx, val, &mut exc);
  if s.is_null() || !exc.is_null() {
    return false;
  }
  let dec = jsstring_to_string(s);
  JSStringRelease(s);

  let src = format!("{path}=BigInt(\"{dec}\");true");
  let r = eval(ctx, &src);
  !r.is_null() && JSValueToBoolean(ctx, r)
}

unsafe fn bigint_to_u64(ctx: JSContextRef, val: JSValueRef) -> u64 {
  let mut exc: JSValueRef = ptr::null();
  let s = JSValueToStringCopy(ctx, val, &mut exc);
  if s.is_null() || !exc.is_null() {
    return 0;
  }
  let dec = jsstring_to_string(s);
  JSStringRelease(s);

  let lo_src = format!("Number(BigInt.asUintN(32,BigInt(\"{dec}\")))");
  let hi_src = format!("Number(BigInt.asUintN(32,BigInt(\"{dec}\")>>32n))");
  let lo = eval(ctx, &lo_src);
  let hi = eval(ctx, &hi_src);
  if lo.is_null() || hi.is_null() {
    return 0;
  }
  let lo_n = JSValueToNumber(ctx, lo, &mut exc) as u64;
  let hi_n = JSValueToNumber(ctx, hi, &mut exc) as u64;
  (hi_n << 32) | (lo_n & 0xFFFF_FFFF)
}

unsafe fn jsstring_to_string(s: JSStringRef) -> std::string::String {
  let cap = JSStringGetMaximumUTF8CStringSize(s);
  if cap == 0 {
    return std::string::String::new();
  }
  let mut buf = vec![0u8; cap];
  let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, cap);
  if n == 0 {
    return std::string::String::new();
  }

  buf.truncate(n.saturating_sub(1));
  std::string::String::from_utf8_lossy(&buf).into_owned()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__WordCount(this: *const BigInt) -> int {
  let ctx = current_ctx();
  let val = jsval(this);
  if ctx.is_null() || val.is_null() {
    return 0;
  }

  let dec = unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, val, &mut exc);
    if s.is_null() || !exc.is_null() {
      return 0;
    }
    let d = jsstring_to_string(s);
    JSStringRelease(s);
    d
  };
  let src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
  );
  let r = unsafe { eval(ctx, &src) };
  if r.is_null() {
    return 0;
  }
  let mut exc: JSValueRef = ptr::null();
  let n = unsafe { JSValueToNumber(ctx, r, &mut exc) };
  n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BigInt__ToWordsArray(
  this: *const BigInt,
  sign_bit: *mut int,
  word_count: *mut int,
  words: *mut u64,
) {
  let ctx = current_ctx();
  let val = jsval(this);
  let avail = if word_count.is_null() {
    0
  } else {
    unsafe { *word_count }.max(0) as usize
  };
  if ctx.is_null() || val.is_null() {
    if !sign_bit.is_null() {
      unsafe { *sign_bit = 0 };
    }
    if !word_count.is_null() {
      unsafe { *word_count = 0 };
    }
    return;
  }
  let dec = unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, val, &mut exc);
    if s.is_null() || !exc.is_null() {
      if !sign_bit.is_null() {
        *sign_bit = 0;
      }
      if !word_count.is_null() {
        *word_count = 0;
      }
      return;
    }
    let d = jsstring_to_string(s);
    JSStringRelease(s);
    d
  };
  let neg = dec.starts_with('-');
  if !sign_bit.is_null() {
    unsafe { *sign_bit = if neg { 1 } else { 0 } };
  }

  let total_src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;let c=0;while(x>0n){{x>>=64n;c++;}}return c;}})()"
  );
  let total = unsafe {
    let r = eval(ctx, &total_src);
    if r.is_null() {
      0usize
    } else {
      let mut exc: JSValueRef = ptr::null();
      JSValueToNumber(ctx, r, &mut exc) as usize
    }
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

unsafe fn bigint_word_at(ctx: JSContextRef, dec: &str, i: usize) -> u64 {
  let shift = 64u64 * i as u64;
  let lo_src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return Number(BigInt.asUintN(32,x>>{shift}n));}})()"
  );
  let hi_src = format!(
    "(()=>{{let x=BigInt(\"{dec}\");if(x<0n)x=-x;return Number(BigInt.asUintN(32,x>>{}n));}})()",
    shift + 32
  );
  let lo = eval(ctx, &lo_src);
  let hi = eval(ctx, &hi_src);
  if lo.is_null() || hi.is_null() {
    return 0;
  }
  let mut exc: JSValueRef = ptr::null();
  let lo_n = JSValueToNumber(ctx, lo, &mut exc) as u64;
  let hi_n = JSValueToNumber(ctx, hi, &mut exc) as u64;
  (hi_n << 32) | (lo_n & 0xFFFF_FFFF)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__ForApi(
  isolate: *mut RealIsolate,
  name: *const V8String,
) -> *const Private {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if name.is_null() {
    std::string::String::new()
  } else {
    unsafe { jsvalue_to_desc(ctx, jsval(name)) }
  };

  let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
  let v =
    unsafe { eval(ctx, &format!("Symbol.for(\"v82jsc_private:{escaped}\")")) };
  intern_ctx::<Private>(ctx, v)
}

unsafe fn jsvalue_to_desc(
  ctx: JSContextRef,
  v: JSValueRef,
) -> std::string::String {
  let mut exc: JSValueRef = ptr::null();
  let s = JSValueToStringCopy(ctx, v, &mut exc);
  if s.is_null() || !exc.is_null() {
    return std::string::String::new();
  }
  let d = jsstring_to_string(s);
  JSStringRelease(s);
  d
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__For(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if description.is_null() {
    std::string::String::new()
  } else {
    unsafe { jsvalue_to_desc(ctx, jsval(description)) }
  };
  let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
  let v = unsafe { eval(ctx, &format!("Symbol.for(\"{escaped}\")")) };
  intern_ctx::<Symbol>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__ForApi(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let desc = if description.is_null() {
    std::string::String::new()
  } else {
    unsafe { jsvalue_to_desc(ctx, jsval(description)) }
  };
  let escaped = desc.replace('\\', "\\\\").replace('"', "\\\"");
  // `Symbol::ForApi` keeps its OWN registry, distinct from the public
  // `Symbol.for` table — same description returns the same symbol, but it must
  // never alias a `Symbol.for(...)` result nor a fresh `Symbol(...)`. Back it
  // with a hidden Map on the context's global, populated with plain `Symbol()`
  // values (so `for_api(d) != for(d)` and `for_api(d) != Symbol(d)`).
  let v = unsafe {
    eval(
      ctx,
      &format!(
        "(function(d){{\
           var g=globalThis;\
           var m=g.__v8_api_symbols__||(g.__v8_api_symbols__=new Map());\
           if(m.has(d))return m.get(d);\
           var s=Symbol(d);m.set(d,s);return s;\
         }})(\"{escaped}\")"
      ),
    )
  };
  intern_ctx::<Symbol>(ctx, v)
}

macro_rules! well_known_symbol {
  ($fn_name:ident, $js:literal) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(isolate: *mut RealIsolate) -> *const Symbol {
      let st = iso_state(isolate);
      let ctx =
        st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
      if ctx.is_null() {
        return ptr::null();
      }
      let v = unsafe { eval(ctx, $js) };
      intern_ctx::<Symbol>(ctx, v)
    }
  };
}

well_known_symbol!(v8__Symbol__GetAsyncIterator, "Symbol.asyncIterator");
well_known_symbol!(v8__Symbol__GetHasInstance, "Symbol.hasInstance");
well_known_symbol!(
  v8__Symbol__GetIsConcatSpreadable,
  "Symbol.isConcatSpreadable"
);
well_known_symbol!(v8__Symbol__GetIterator, "Symbol.iterator");
well_known_symbol!(v8__Symbol__GetMatch, "Symbol.match");
well_known_symbol!(v8__Symbol__GetReplace, "Symbol.replace");
well_known_symbol!(v8__Symbol__GetSearch, "Symbol.search");
well_known_symbol!(v8__Symbol__GetSplit, "Symbol.split");
well_known_symbol!(v8__Symbol__GetToPrimitive, "Symbol.toPrimitive");
well_known_symbol!(v8__Symbol__GetToStringTag, "Symbol.toStringTag");
well_known_symbol!(v8__Symbol__GetUnscopables, "Symbol.unscopables");

// `Symbol.dispose` and `Symbol.asyncDispose` (explicit resource management)
// are missing from JavaScriptCore builds that predate the proposal. Install
// the symbol on the realm's `Symbol` the first time it is asked for, so the
// handle returned is the same one JS sees as `Symbol.dispose`.
well_known_symbol!(
  v8__Symbol__GetDispose,
  "(typeof Symbol.dispose === 'symbol' ? Symbol.dispose : \
   (Object.defineProperty(Symbol, 'dispose', \
     {value: Symbol('Symbol.dispose')}), Symbol.dispose))"
);
well_known_symbol!(
  v8__Symbol__GetAsyncDispose,
  "(typeof Symbol.asyncDispose === 'symbol' ? Symbol.asyncDispose : \
   (Object.defineProperty(Symbol, 'asyncDispose', \
     {value: Symbol('Symbol.asyncDispose')}), Symbol.asyncDispose))"
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__EQ(this: *const Data, other: *const Data) -> bool {
  let ctx = current_ctx();
  let a = jsval(this);
  let b = jsval(other);
  if a == b {
    return true;
  }
  if ctx.is_null() || a.is_null() || b.is_null() {
    return false;
  }
  unsafe { JSValueIsStrictEqual(ctx, a, b) }
}

/// V8 rejects a handle whose heap object lives in another isolate. JSC's
/// public API cannot tell which `JSContextGroupRef` a value belongs to, and
/// rusty_v8 calls this on every `Local` dereference in debug builds, so every
/// handle is accepted.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsCompatibleWithIsolate(
  _this: *const Data,
  _isolate: *const RealIsolate,
) -> bool {
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsValue(this: *const Data) -> bool {
  !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrimitive(this: *const Data) -> bool {
  let ctx = current_ctx();
  let v = jsval(this);
  if ctx.is_null() || v.is_null() {
    return false;
  }

  !unsafe { JSValueIsObject(ctx, v) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFunctionTemplate(this: *const Data) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsDictionaryTemplate(this: *const Data) -> bool {
  crate::jsc::function::is_dictionary_template(
    this as *const std::os::raw::c_void,
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModule(this: *const Data) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsModuleRequest(this: *const Data) -> bool {
  let ctx = crate::jsc::core::current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  unsafe {
    let v = this as crate::jsc::jsc_sys::JSValueRef;
    if !JSValueIsObject(ctx, v) {
      return false;
    }
    let obj = v as crate::jsc::jsc_sys::JSObjectRef;
    let key = JSStringCreateWithUTF8CString(c"__v8jsc_module_request".as_ptr());
    let mut exc: crate::jsc::jsc_sys::JSValueRef = std::ptr::null();
    let prop = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    !prop.is_null() && JSValueToBoolean(ctx, prop)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__New(
  isolate: *mut RealIsolate,
  description: *const V8String,
) -> *const Symbol {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let desc_s = if description.is_null() {
      JSStringCreateWithUTF8CString(b"\0".as_ptr() as *const c_char)
    } else {
      let mut exc: JSValueRef = ptr::null();
      JSValueToStringCopy(ctx, jsval(description), &mut exc)
    };
    let v = JSValueMakeSymbol(ctx, desc_s);
    if !desc_s.is_null() {
      JSStringRelease(desc_s);
    }
    intern_ctx::<Symbol>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Symbol__Description(
  this: *const Symbol,
  isolate: *mut RealIsolate,
) -> *const Value {
  let ctx = if isolate.is_null() {
    current_ctx()
  } else {
    iso_state(isolate)
      .contexts
      .last()
      .copied()
      .unwrap_or(ptr::null_mut()) as JSContextRef
  };
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src = b"(function(s){return s.description;})\0";
    let fs = JSStringCreateWithUTF8CString(src.as_ptr() as *const c_char);
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [jsval(this)];
    let v = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || v.is_null() {
      return ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__New(
  isolate: *mut RealIsolate,
  name: *const V8String,
) -> *const Private {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let desc_s = if name.is_null() {
      JSStringCreateWithUTF8CString(b"v8jsc_private\0".as_ptr() as *const c_char)
    } else {
      let mut exc: JSValueRef = ptr::null();
      JSValueToStringCopy(ctx, jsval(name), &mut exc)
    };
    let v = JSValueMakeSymbol(ctx, desc_s);
    if !desc_s.is_null() {
      JSStringRelease(desc_s);
    }
    intern_ctx::<Private>(ctx, v)
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
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsPrivate(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Private__Name(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}
