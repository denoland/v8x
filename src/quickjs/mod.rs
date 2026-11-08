//! QuickJS-ng engine backend for the v8 C-ABI compat layer.
//!
//! Mirrors the JSC backend's structure: `quickjs_sys` holds the raw QuickJS-ng
//! FFI (adapted from denoland/deno#34033's qjs_v8_compat), `shims` holds the
//! `v8__*` C-ABI definitions (currently auto-generated stubs; real
//! implementations land incrementally, same pattern as the JSC backend).

mod allocator;
mod arraybuffer;
mod cli_extra;
mod core;
mod exception;
mod function;
mod init;
mod inspector;
mod isolate;
mod locker;
mod misc;
mod module;
mod object;
mod primitive;
mod property;
pub(crate) mod quickjs_sys;
mod runtime;
mod serializer;
mod shims;
mod simdutf;
pub(crate) mod snapshot;
mod string;
mod temporal;
mod value;
mod wasm;

#[cfg(test)]
mod raw_smoke_test {
  use super::quickjs_sys::*;
  use std::ffi::CStr;
  use std::ffi::CString;
  use std::ptr;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;

  static PREPARE_STACK_CALLS: AtomicUsize = AtomicUsize::new(0);
  static RECAPTURE_PREPARE_STACK_CALLS: AtomicUsize = AtomicUsize::new(0);
  static UNHANDLED_REJECTIONS: AtomicUsize = AtomicUsize::new(0);
  static HANDLED_REJECTIONS: AtomicUsize = AtomicUsize::new(0);

  unsafe extern "C" {
    fn v82jsc_clear_error_backtrace(ctx: *mut JSContext, error: JSValue);
    fn v82jsc_ensure_error_backtrace(ctx: *mut JSContext, error: JSValue);
  }

  unsafe extern "C" fn test_prepare_stack_trace(
    ctx: *mut JSContext,
    _this: JSValue,
    _argc: std::ffi::c_int,
    _argv: *mut JSValue,
  ) -> JSValue {
    PREPARE_STACK_CALLS.fetch_add(1, Ordering::SeqCst);
    unsafe { JS_NewString(ctx, c"embedder stack".as_ptr()) }
  }

  unsafe extern "C" fn test_recapture_prepare_stack_trace(
    ctx: *mut JSContext,
    _this: JSValue,
    _argc: std::ffi::c_int,
    _argv: *mut JSValue,
  ) -> JSValue {
    RECAPTURE_PREPARE_STACK_CALLS.fetch_add(1, Ordering::SeqCst);
    unsafe { JS_NewString(ctx, c"embedder stack".as_ptr()) }
  }

  unsafe extern "C" fn test_forward_prepare_stack_trace(
    ctx: *mut JSContext,
    _this: JSValue,
    argc: std::ffi::c_int,
    argv: *mut JSValue,
  ) -> JSValue {
    unsafe {
      let global = JS_GetGlobalObject(ctx);
      let formatter = JS_GetPropertyStr(ctx, global, c"formatStack".as_ptr());
      let result = JS_Call(ctx, formatter, global, argc, argv);
      JS_FreeValue(ctx, formatter);
      JS_FreeValue(ctx, global);
      result
    }
  }

  unsafe extern "C" fn test_promise_rejection_tracker(
    _ctx: *mut JSContext,
    _promise: JSValue,
    _reason: JSValue,
    is_handled: std::ffi::c_int,
    _opaque: *mut std::ffi::c_void,
  ) {
    if is_handled == 0 {
      UNHANDLED_REJECTIONS.fetch_add(1, Ordering::SeqCst);
    } else {
      HANDLED_REJECTIONS.fetch_add(1, Ordering::SeqCst);
    }
  }

  unsafe extern "C" fn test_thrower(
    ctx: *mut JSContext,
    _this: JSValue,
    _argc: std::ffi::c_int,
    _argv: *mut JSValue,
  ) -> JSValue {
    unsafe {
      let global = JS_GetGlobalObject(ctx);
      let constructor = JS_GetPropertyStr(ctx, global, c"TypeError".as_ptr());
      JS_FreeValue(ctx, global);
      let mut message = JS_NewString(ctx, c"boom".as_ptr());
      let error = JS_CallConstructor(ctx, constructor, 1, &mut message);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, constructor);
      v82jsc_clear_error_backtrace(ctx, error);
      v82jsc_ensure_error_backtrace(ctx, error);
      JS_Throw(ctx, error)
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn eval_one_plus_one_raw() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let code = CString::new("1 + 1").unwrap();
      let fname = CString::new("<eval>").unwrap();
      let result =
        JS_Eval(ctx, code.as_ptr(), 4, fname.as_ptr(), JS_EVAL_TYPE_GLOBAL);
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut n: f64 = 0.0;
      JS_ToFloat64(ctx, &mut n, result);
      assert_eq!(n, 2.0);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
      let _ = ptr::null::<u8>();
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn eval_no_side_effect_rejects_global_write() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let setup = c"globalThis.sideEffect = 0";
      let result = JS_Eval(
        ctx,
        setup.as_ptr(),
        setup.to_bytes().len(),
        c"<setup>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION);
      JS_FreeValue(ctx, result);

      let expression = c"(globalThis.sideEffect++, 0)";
      let result = v82jsc_eval_no_side_effect(
        ctx,
        expression.as_ptr(),
        expression.to_bytes().len(),
        c"<preview>".as_ptr(),
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);
      let error = JS_GetException(ctx);
      JS_FreeValue(ctx, error);

      let read = c"globalThis.sideEffect";
      let result = JS_Eval(
        ctx,
        read.as_ptr(),
        read.to_bytes().len(),
        c"<read>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      let mut value = -1;
      assert_eq!(JS_ToInt32(ctx, &mut value, result), 0);
      assert_eq!(value, 0);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn nested_call_error_uses_inner_call_location() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let global = JS_GetGlobalObject(ctx);
      let thrower = JS_NewCFunction(ctx, test_thrower, c"thrower".as_ptr(), 0);
      assert_eq!(
        JS_SetPropertyStr(ctx, global, c"thrower".as_ptr(), thrower),
        1
      );
      JS_FreeValue(ctx, global);

      let source = c"function wrapper() { thrower() }\nconst api = { wrapper }; function outer(value) {}\nouter(api.wrapper())";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"nested-call.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);
      let error = JS_GetException(ctx);
      let stack = JS_GetPropertyStr(ctx, error, c"stack".as_ptr());
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, stack);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert!(actual.contains("nested-call.js:3:11"), "{actual}");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, stack);
      JS_FreeValue(ctx, error);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn weakref_targets_are_kept_until_cleared() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let create = c"globalThis.ref = new WeakRef({});
        ref.deref() !== undefined";
      let result = JS_Eval(
        ctx,
        create.as_ptr(),
        create.to_bytes().len(),
        c"weakref-create.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(JS_ToBool(ctx, result), 1);
      JS_FreeValue(ctx, result);

      JS_RunGC(rt);
      let retained = c"ref.deref() !== undefined";
      let result = JS_Eval(
        ctx,
        retained.as_ptr(),
        retained.to_bytes().len(),
        c"weakref-retained.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(JS_ToBool(ctx, result), 1);
      JS_FreeValue(ctx, result);

      JS_ClearKeptObjects(rt);
      JS_RunGC(rt);
      let cleared = c"ref.deref() === undefined";
      let result = JS_Eval(
        ctx,
        cleared.as_ptr(),
        cleared.to_bytes().len(),
        c"weakref-cleared.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(JS_ToBool(ctx, result), 1);
      JS_FreeValue(ctx, result);

      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn weak_collection_targets_are_kept_until_cleared() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      for source in [c"new WeakSet([{}])", c"new WeakMap([[{}, 1]])"] {
        let collection = JS_Eval(
          ctx,
          source.as_ptr(),
          source.to_bytes().len(),
          c"weak-collection.js".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        );
        assert_ne!(collection.tag, JS_TAG_EXCEPTION);

        let preview =
          js_v82jsc_iterator_preview(ctx, collection, std::ptr::null_mut());
        assert_ne!(preview.tag, JS_TAG_EXCEPTION);
        let length = JS_GetPropertyStr(ctx, preview, c"length".as_ptr());
        let mut length_value = 0;
        assert_eq!(JS_ToInt32(ctx, &mut length_value, length), 0);
        assert!(length_value > 0);
        JS_FreeValue(ctx, length);
        JS_FreeValue(ctx, preview);

        JS_ClearKeptObjects(rt);
        JS_RunGC(rt);
        let preview =
          js_v82jsc_iterator_preview(ctx, collection, std::ptr::null_mut());
        assert_ne!(preview.tag, JS_TAG_EXCEPTION);
        let length = JS_GetPropertyStr(ctx, preview, c"length".as_ptr());
        let mut length_value = -1;
        assert_eq!(JS_ToInt32(ctx, &mut length_value, length), 0);
        assert_eq!(length_value, 0);
        JS_FreeValue(ctx, length);
        JS_FreeValue(ctx, preview);
        JS_FreeValue(ctx, collection);
      }

      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn native_weakref_does_not_keep_target_alive() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let target = JS_NewObject(ctx);
      let weak = v82jsc_new_weak_ref(ctx, target);
      assert!(v82jsc_weak_ref_is_live(weak));

      JS_FreeValue(ctx, target);
      JS_RunGC(rt);
      assert!(!v82jsc_weak_ref_is_live(weak));

      JS_FreeValue(ctx, weak);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn heap_usage_counts_fast_array_holes() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let mut before = JSMemoryUsage::default();
      JS_ComputeMemoryUsage(rt, &mut before);
      let result = JS_Eval(
        ctx,
        c"globalThis.holes = new Array(1_000_000)".as_ptr(),
        c"globalThis.holes = new Array(1_000_000)".to_bytes().len(),
        c"array-holes.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION);
      JS_FreeValue(ctx, result);

      let mut after = JSMemoryUsage::default();
      JS_ComputeMemoryUsage(rt, &mut after);
      assert!(after.memory_used_size - before.memory_used_size >= 8_000_000);

      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn synthetic_module_namespace_has_module_semantics() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let namespace = v82jsc_new_module_namespace(ctx);
      assert!(namespace.tag != JS_TAG_EXCEPTION);
      assert_eq!(
        v82jsc_module_namespace_set(
          ctx,
          namespace,
          c"default".as_ptr(),
          jsv_int32(1),
        ),
        0
      );

      let global = JS_GetGlobalObject(ctx);
      assert_eq!(
        JS_SetPropertyStr(
          ctx,
          global,
          c"namespace".as_ptr(),
          JS_DupValue(ctx, namespace),
        ),
        1
      );
      JS_FreeValue(ctx, global);

      let source = c"Object.preventExtensions(namespace);
        [Object.prototype.toString.call(namespace),
         Object.getPrototypeOf(namespace) === null,
         Object.isExtensible(namespace),
         Reflect.set(namespace, 'default', 2),
         namespace.default,
         Object.getOwnPropertyDescriptor(namespace, 'default').writable
        ].join(':')";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"synthetic-module-namespace.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "namespace inspection threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "[object Module]:true:false:false:1:true");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeValue(ctx, namespace);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn embedder_prepare_stack_is_lazy_and_separate_from_user_hook() {
    unsafe {
      PREPARE_STACK_CALLS.store(0, Ordering::SeqCst);
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let callback = JS_NewCFunction(
        ctx,
        test_prepare_stack_trace,
        c"prepareStackTrace".as_ptr(),
        2,
      );
      JS_SetPrepareStackTraceCallback(ctx, callback);
      JS_FreeValue(ctx, callback);

      let setup = c"Error.prepareStackTrace = () => 'user stack'; globalThis.error = new Error('boom'); typeof Error.prepareStackTrace";
      let result = JS_Eval(
        ctx,
        setup.as_ptr(),
        setup.to_bytes().len(),
        c"prepare-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "setup threw");
      assert_eq!(PREPARE_STACK_CALLS.load(Ordering::SeqCst), 0);
      JS_FreeValue(ctx, result);

      let result = JS_Eval(
        ctx,
        c"error.stack".as_ptr(),
        c"error.stack".to_bytes().len(),
        c"prepare-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "stack access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "embedder stack");
      assert_eq!(PREPARE_STACK_CALLS.load(Ordering::SeqCst), 1);

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn capture_stack_trace_replaces_error_stack_lazily() {
    unsafe {
      RECAPTURE_PREPARE_STACK_CALLS.store(0, Ordering::SeqCst);
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let callback = JS_NewCFunction(
        ctx,
        test_recapture_prepare_stack_trace,
        c"prepareStackTrace".as_ptr(),
        2,
      );
      JS_SetPrepareStackTraceCallback(ctx, callback);
      JS_FreeValue(ctx, callback);

      let setup = c"globalThis.error = new Error('boom');
        function recapture() {
          Error.captureStackTrace(error, recapture);
        }
        recapture();
        typeof Object.getOwnPropertyDescriptor(error, 'stack').get";
      let result = JS_Eval(
        ctx,
        setup.as_ptr(),
        setup.to_bytes().len(),
        c"capture-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "setup threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "function");
      assert_eq!(RECAPTURE_PREPARE_STACK_CALLS.load(Ordering::SeqCst), 0);
      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);

      let result = JS_Eval(
        ctx,
        c"error.stack".as_ptr(),
        c"error.stack".to_bytes().len(),
        c"capture-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "stack access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "embedder stack");
      assert_eq!(RECAPTURE_PREPARE_STACK_CALLS.load(Ordering::SeqCst), 1);

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn embedder_prepare_stack_receives_async_frame_lazily() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let callback = JS_NewCFunction(
        ctx,
        test_forward_prepare_stack_trace,
        c"prepareStackTrace".as_ptr(),
        2,
      );
      JS_SetPrepareStackTraceCallback(ctx, callback);
      JS_FreeValue(ctx, callback);

      let source = c"globalThis.formatStack = (_, sites) => sites.map((site) =>
          `${site.isAsync()}:${site.getFileName()}:${site.getLineNumber()}`).join('|');
        globalThis.result = 'pending';
        (async function parent() {
          const error = new Error('boom');
          try {
            await Promise.reject(error);
          } catch (error) {
            globalThis.result = error.stack;
          }
        })();";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"lazy-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      JS_FreeValue(ctx, result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let result = JS_Eval(
        ctx,
        c"result".as_ptr(),
        c"result".to_bytes().len(),
        c"lazy-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "result access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert!(
        actual.contains("true:lazy-async-stack.js:"),
        "embedder callback missed async frame: {actual}"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn async_rejection_preserves_custom_stack_getter() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"let getter = () => 'custom stack';
        globalThis.result = 'pending';
        (async function outer() {
          const error = new Error('boom');
          Object.defineProperty(error, 'stack', { get: getter });
          try {
            await Promise.reject(error);
          } catch (error) {
            const descriptor = Object.getOwnPropertyDescriptor(error, 'stack');
            globalThis.result = `${descriptor.get === getter}:${error.stack}`;
          }
        })();";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"custom-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      JS_FreeValue(ctx, result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let result = JS_Eval(
        ctx,
        c"result".as_ptr(),
        c"result".to_bytes().len(),
        c"custom-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "result access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "true:custom stack");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn async_rejection_preserves_stackless_errors() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"globalThis.result = 'pending';
        globalThis.initial = 'pending';
        const promise = Promise.any([]);
        promise.catch((error) => {
          globalThis.initial = error.stack;
        });
        (async function inspectAggregateError() {
          try {
            await promise;
          } catch (error) {
            globalThis.result = JSON.stringify([
              initial,
              error.stack,
              error.message,
            ]);
          }
        })();";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"stackless-async-error.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION, "eval threw");
      JS_FreeValue(ctx, result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let result = JS_Eval(
        ctx,
        c"result".as_ptr(),
        c"result".to_bytes().len(),
        c"stackless-async-error.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION, "result access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(
        actual,
        "[\"AggregateError: All promises were rejected\",\"AggregateError: All promises were rejected\",\"All promises were rejected\"]"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn async_rejection_stack_deduplicates_await_site() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"globalThis.sameSiteStack = 'pending';
        globalThis.parentStack = 'pending';
        (async function sameSite() {
          try {
            await Promise.reject(new Error('same site'));
          } catch (error) {
            globalThis.sameSiteStack = error.stack;
          }
        })();
        async function child() { await 0; throw new Error('child'); }
        (async function parent() { try { await child(); } catch (error) {
          globalThis.parentStack = error.stack;
        } })();";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"async-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      JS_FreeValue(ctx, result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let result = JS_Eval(
        ctx,
        c"sameSiteStack + '\\n---\\n' + parentStack".as_ptr(),
        c"sameSiteStack + '\\n---\\n' + parentStack"
          .to_bytes()
          .len(),
        c"async-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "result access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      let (same_site, parent) = actual.split_once("\n---\n").unwrap();
      assert_eq!(
        same_site.matches("async-stack-test.js:5:").count(),
        1,
        "same await site was duplicated:\n{same_site}"
      );
      assert!(
        parent.contains("at async parent (async-stack-test.js:11:"),
        "missing async parent frame:\n{parent}"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn marking_rejected_promise_handled_notifies_tracker() {
    unsafe {
      UNHANDLED_REJECTIONS.store(0, Ordering::SeqCst);
      HANDLED_REJECTIONS.store(0, Ordering::SeqCst);
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      JS_SetHostPromiseRejectionTracker(
        rt,
        Some(test_promise_rejection_tracker),
        ptr::null_mut(),
      );
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"Promise.reject('boom')";
      let promise = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"mark-promise-handled.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(promise.tag != JS_TAG_EXCEPTION, "eval threw");
      assert_eq!(UNHANDLED_REJECTIONS.load(Ordering::SeqCst), 1);
      assert_eq!(HANDLED_REJECTIONS.load(Ordering::SeqCst), 0);

      JS_PromiseMarkAsHandled(ctx, promise);
      JS_PromiseMarkAsHandled(ctx, promise);
      assert_eq!(UNHANDLED_REJECTIONS.load(Ordering::SeqCst), 1);
      assert_eq!(HANDLED_REJECTIONS.load(Ordering::SeqCst), 1);

      JS_FreeValue(ctx, promise);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn module_evaluation_exception_remains_available() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"throw new Error('boom')";
      let module = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"throwing-module.js".as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      );
      assert_eq!(module.tag, JS_TAG_MODULE);
      let module_def = module.u.ptr.cast::<JSModuleDef>();

      let result = JS_EvalFunction(ctx, module);
      assert!(JS_IsPromise(result));
      assert_eq!(JS_PromiseState(ctx, result), 2);

      let exception = v82jsc_module_get_exception(ctx, module_def);
      assert_eq!(exception.tag, JS_TAG_OBJECT);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let message_text = JS_ToCString(ctx, message);
      assert!(!message_text.is_null());
      assert_eq!(CStr::from_ptr(message_text).to_bytes(), b"boom");

      let global = JS_GetGlobalObject(ctx);
      assert_eq!(
        JS_SetPropertyStr(ctx, global, c"modulePromise".as_ptr(), result),
        1
      );
      JS_FreeValue(ctx, global);

      let consumer = c"globalThis.moduleStack = 'pending';
        (async function consumer() {
          try {
            await modulePromise;
          } catch (error) {
            globalThis.moduleStack = error.stack;
          }
        })();";
      let consumer_result = JS_Eval(
        ctx,
        consumer.as_ptr(),
        consumer.to_bytes().len(),
        c"module-consumer.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(consumer_result.tag != JS_TAG_EXCEPTION, "consumer threw");
      JS_FreeValue(ctx, consumer_result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let stack = JS_Eval(
        ctx,
        c"moduleStack".as_ptr(),
        c"moduleStack".to_bytes().len(),
        c"module-consumer.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(stack.tag != JS_TAG_EXCEPTION, "stack access threw");
      let stack_cstr = JS_ToCString(ctx, stack);
      assert!(!stack_cstr.is_null());
      let stack_text = CStr::from_ptr(stack_cstr).to_string_lossy();
      assert!(stack_text.contains("throwing-module.js:1"));
      assert!(!stack_text.contains("module-consumer.js"));
      JS_FreeCString(ctx, stack_cstr);
      JS_FreeValue(ctx, stack);

      JS_FreeCString(ctx, message_text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn bigint_as_uint_n_wraps_negative_values() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"[
        BigInt.asUintN(32, -1n) === 4294967295n,
        BigInt.asUintN(64, -1n) === 18446744073709551615n,
        BigInt.asUintN(100, -1n) === (1n << 100n) - 1n,
        BigInt.asUintN(32, -(1n << 80n) - 1n) === 4294967295n,
        BigInt.asUintN(64, -(1n << 80n) - 1233n) === 18446744073709550383n,
        BigInt.asIntN(64, -1n) === -1n,
        BigInt.asUintN(64, 1n) === 1n,
      ].every(Boolean)";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"bigint-asuintn.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_ne!(result.tag, JS_TAG_EXCEPTION);
      assert_eq!(JS_ToBool(ctx, result), 1);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn module_array_destructuring_exports_all_bindings() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"export const [libPrefix, libSuffix] = ['lib', 'dylib']";
      let module = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"array-destructuring-module.js".as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      );
      assert_eq!(module.tag, JS_TAG_MODULE);
      let module_def = module.u.ptr.cast::<JSModuleDef>();

      let evaluated = JS_EvalFunction(ctx, JS_DupValue(ctx, module));
      assert_ne!(evaluated.tag, JS_TAG_EXCEPTION);
      let namespace = JS_GetModuleNamespace(ctx, module_def);
      assert_ne!(namespace.tag, JS_TAG_EXCEPTION);

      let prefix = JS_GetPropertyStr(ctx, namespace, c"libPrefix".as_ptr());
      let suffix = JS_GetPropertyStr(ctx, namespace, c"libSuffix".as_ptr());
      let prefix_text = JS_ToCString(ctx, prefix);
      let suffix_text = JS_ToCString(ctx, suffix);
      assert!(!prefix_text.is_null());
      assert!(!suffix_text.is_null());
      assert_eq!(CStr::from_ptr(prefix_text).to_bytes(), b"lib");
      assert_eq!(CStr::from_ptr(suffix_text).to_bytes(), b"dylib");

      JS_FreeCString(ctx, suffix_text);
      JS_FreeCString(ctx, prefix_text);
      JS_FreeValue(ctx, suffix);
      JS_FreeValue(ctx, prefix);
      JS_FreeValue(ctx, namespace);
      JS_FreeValue(ctx, evaluated);
      JS_FreeValue(ctx, module);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn missing_member_call_names_callee() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"globalThis.Deno = {}; Deno.openKv();";
      super::core::register_script_source(
        "missing-member.js",
        source.to_str().unwrap(),
      );
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"missing-member.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);

      let exception = JS_GetException(ctx);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let message_text = JS_ToCString(ctx, message);
      assert!(!message_text.is_null());
      assert_eq!(
        CStr::from_ptr(message_text).to_bytes(),
        b"Deno.openKv is not a function"
      );

      JS_FreeCString(ctx, message_text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn eval_preserves_embedded_nul_raw() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = b"globalThis.value = 1; // virtual\0module\n\
                     globalThis.value = 2; value;\0";
      let result = JS_Eval(
        ctx,
        source.as_ptr() as *const std::ffi::c_char,
        source.len() - 1,
        c"nul-source-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 2);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn strict_class_method_allows_reserved_property_name() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "'use strict'; class Scheduler { yield() { return 1; } } new Scheduler().yield();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"strict-class-method.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 1);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn repl_mode_redeclares_let_but_not_const() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      for source in [c"let foo = 0;", c"let foo = foo + 1;"] {
        let result = v82jsc_eval_repl(
          ctx,
          source.as_ptr(),
          source.to_bytes().len(),
          c"<repl>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        );
        assert_ne!(result.tag, JS_TAG_EXCEPTION, "REPL let threw");
        JS_FreeValue(ctx, result);
      }

      let value = JS_Eval(
        ctx,
        c"foo".as_ptr(),
        c"foo".to_bytes().len(),
        c"<repl>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, value), 0);
      assert_eq!(actual, 1);
      JS_FreeValue(ctx, value);

      let source = c"const foo = 2;";
      let result = v82jsc_eval_repl(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"<repl>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);
      let exception = JS_GetException(ctx);
      JS_FreeValue(ctx, exception);

      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn unterminated_eval_uses_v8_error_message() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"eval('{')";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"eval-eof-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);

      let exception = JS_GetException(ctx);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let text = JS_ToCString(ctx, message);
      assert!(!text.is_null());
      assert_eq!(
        CStr::from_ptr(text).to_str().unwrap(),
        "Unexpected end of input"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn readonly_object_assignment_uses_v8_message() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"'use strict';
        const object = {};
        Object.defineProperty(object, 'self', { value: object });
        object.self = 1;";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"readonly-object.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);

      let exception = JS_GetException(ctx);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let text = JS_ToCString(ctx, message);
      assert!(!text.is_null());
      assert_eq!(
        CStr::from_ptr(text).to_bytes(),
        b"Cannot assign to read only property 'self' of object '#<Object>'"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn nonextensible_assignment_names_property() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source =
        c"'use strict'; const a = Object.preventExtensions({}); a.c = 1;";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"nonextensible-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);

      let exception = JS_GetException(ctx);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let text = JS_ToCString(ctx, message);
      assert!(!text.is_null());
      assert_eq!(
        CStr::from_ptr(text).to_str().unwrap(),
        "Cannot add property c, object is not extensible"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn class_method_name_does_not_shadow_outer_binding() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "function record(value) { return value + 1; }\n\
         class Gauge {\n\
           #value = 1;\n\
           record() { return record(this.#value); }\n\
         }\n\
         new Gauge().record();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"method-outer-binding.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 2);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn callsite_preserves_receiver_type_and_method() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {\n\
         const site = sites[0];\n\
         return [site.getTypeName(), site.getFunctionName(),\n\
                 site.getMethodName(), site.getThis()[Symbol.toStringTag]].join(':');\n\
         };\n\
         const receiver = {\n\
           [Symbol.toStringTag]: 'BenchContext',\n\
           start() { return new Error().stack; },\n\
         };\n\
         receiver.start();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "BenchContext:start:start:BenchContext");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn callsite_reports_global_receiver_as_toplevel() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {\n\
         const site = sites[0];\n\
         return `${site.getFunctionName()}:${site.isToplevel()}`;\n\
         };\n\
         globalThis.callback = function callback() { return new Error().stack; };\n\
         globalThis.callback();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-toplevel-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "callback:true");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn callsite_reports_native_constructor() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {
         const site = sites.find((site) => site.getFunctionName() === 'Promise');
         return `${site.getFunctionName()}:${site.isConstructor()}:${site.getLineNumber()}:${site.getColumnNumber()}`;
         };
         let error;
         new Promise(() => { error = new Error('fail'); });
         error.stack;",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-constructor-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "Promise:true:null:null");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn property_access_errors_match_v8() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source =
        CString::new("try { undefined.fn } catch (error) { error.message }")
          .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"typeerror-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "Cannot read properties of undefined (reading 'fn')");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn error_stack_accepts_arbitrary_values() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "const error = new Error('x');
         error.stack = undefined;
         const hasOwnStack = Object.hasOwn(error, 'stack');
         Error.prototype.stack = 42;
         `${error.stack}:${hasOwnStack}:${Error.prototype.stack}`;",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"error-stack-value-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "undefined:true:42");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn error_stack_is_an_own_lazy_accessor() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"class DerivedError extends Error {}
        const error = new Error('plain');
        const derived = new DerivedError('derived');
        const descriptor = Object.getOwnPropertyDescriptor(error, 'stack');
        [
          Object.hasOwn(error, 'stack'),
          Object.hasOwn(derived, 'stack'),
          typeof descriptor.get,
          typeof descriptor.set,
          descriptor.enumerable,
          descriptor.configurable,
          derived.stack.startsWith('Error: derived'),
        ].join(':')";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"error-own-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "true:true:function:function:false:true:true");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn function_constructor_preserves_caller_resource_name() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source =
        CString::new("new Function('return new Error().stack')()").unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"function-origin-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert!(
        actual.contains("function-origin-test.js"),
        "generated function stack did not preserve caller resource name: {actual}"
      );
      assert!(
        !actual.contains("<input>"),
        "unexpected synthetic name: {actual}"
      );

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }
}

#[cfg(test)]
#[cfg(feature = "link_quickjs")]
mod api_test {
  use crate as v8;
  use v8::MapFnTo;

  thread_local! {
    static DYNAMIC_IMPORT_HOST_OPTIONS: std::cell::Cell<Option<(bool, usize)>> =
      const { std::cell::Cell::new(None) };
  }

  fn resolve_dynamic_import<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    host_defined_options: v8::Local<'s, v8::Data>,
    _resource_name: v8::Local<'s, v8::Value>,
    _specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
  ) -> Option<v8::Local<'s, v8::Promise>> {
    let options: v8::Local<v8::Value> = unsafe {
      std::mem::transmute::<v8::Local<v8::Data>, v8::Local<v8::Value>>(
        host_defined_options,
      )
    };
    let is_array = options.is_array();
    let length = if is_array {
      let options: v8::Local<v8::PrimitiveArray> = unsafe {
        std::mem::transmute::<v8::Local<v8::Data>, v8::Local<v8::PrimitiveArray>>(
          host_defined_options,
        )
      };
      options.length()
    } else {
      usize::MAX
    };
    DYNAMIC_IMPORT_HOST_OPTIONS
      .with(|state| state.set(Some((is_array, length))));

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let namespace = v8::Object::new(scope);
    resolver.resolve(scope, namespace.into());
    Some(promise)
  }

  fn snapshot_global_setter(
    scope: &mut v8::PinScope,
    key: v8::Local<v8::Name>,
    value: v8::Local<v8::Value>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Boolean>,
  ) -> v8::Intercepted {
    let key = key.to_rust_string_lossy(scope);
    if key == "seen" {
      return v8::Intercepted::kNo;
    }

    let seen = v8::String::new(scope, "seen").unwrap();
    if args
      .holder()
      .create_data_property(scope, seen.into(), value)
      != Some(true)
    {
      return v8::Intercepted::kNo;
    }
    rv.set_bool(true);
    v8::Intercepted::kYes
  }

  #[test]
  fn qjs_api_eval() {
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    {
      let isolate = &mut v8::Isolate::new(Default::default());

      let scope = std::pin::pin!(v8::HandleScope::new(isolate));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);

      let code = v8::String::new(scope, "1 + 1").unwrap();
      let script = v8::Script::compile(scope, code, None).unwrap();
      let result = script.run(scope).unwrap();
      assert_eq!(result.to_rust_string_lossy(scope), "2");
    }

    assert_temporal_support();
    assert_intl_basics();
    assert_finalization_registry_token_is_weak();
    assert_unbound_script_preserves_origin();
    assert_continuation_data_survives_await();
    assert_snapshot_context_data_preserves_global_identity();
    assert_snapshot_global_template_handlers();
    assert_detached_array_buffer_view_contents();
    assert_detached_backing_store_deleter_runs_once();
    assert_large_array_buffers();
    assert_empty_shared_backing_store_has_null_data();
    assert_backing_store_survives_isolate();
    assert_global_drop_uses_owning_isolate();
    assert_internal_globals_are_not_enumerable();
    assert_suspended_async_capture_survives_gc();
    assert_rejected_await_capture_survives_gc();
    assert_async_iterator_close_is_awaited();
    assert_native_promise_then_ignores_monkeypatch();
    assert_promise_hooks_follow_continuations();
    assert_bigint_words_preserve_u64_max();
    assert_wasm_streaming_respects_explicit_microtasks();
    assert_duplicate_module_requests_resolve_once();
    assert_dynamic_import_gets_empty_host_options();

    unsafe {
      v8::V8::dispose();
    }
    v8::V8::dispose_platform();
  }

  fn assert_dynamic_import_gets_empty_host_options() {
    DYNAMIC_IMPORT_HOST_OPTIONS.with(|state| state.set(None));
    let isolate = &mut v8::Isolate::new(Default::default());
    isolate.set_host_import_module_dynamically_callback(resolve_dynamic_import);
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let source = v8::String::new(scope, "import('dependency')").unwrap();
    let promise = v8::Local::<v8::Promise>::try_from(
      v8::Script::compile(scope, source, None)
        .unwrap()
        .run(scope)
        .unwrap(),
    )
    .unwrap();
    scope.perform_microtask_checkpoint();

    assert_eq!(promise.state(), v8::PromiseState::Fulfilled);
    assert_eq!(
      DYNAMIC_IMPORT_HOST_OPTIONS.with(|state| state.get()),
      Some((true, 0))
    );
  }

  fn assert_detached_array_buffer_view_contents() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let buffer = v8::ArrayBuffer::new(scope, 16);
    let view = v8::Uint8Array::new(scope, buffer, 0, 16).unwrap();
    assert!(buffer.detach(None).unwrap());

    assert_eq!(view.byte_length(), 0);
    assert_eq!(view.byte_offset(), 0);
    let detached_buffer = view.buffer(scope).unwrap();
    assert_eq!(detached_buffer.byte_length(), 0);
    assert!(detached_buffer.data().is_none());

    let mut storage = [0; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
    assert!(view.get_contents(&mut storage).is_empty());

    let code = v8::String::new(scope, "40 + 2").unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    assert_eq!(script.run(scope).unwrap().integer_value(scope), Some(42));
  }

  fn assert_detached_backing_store_deleter_runs_once() {
    static DELETER_CALLS: std::sync::atomic::AtomicUsize =
      std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn count_deleter(
      _data: *mut std::ffi::c_void,
      _byte_length: usize,
      _deleter_data: *mut std::ffi::c_void,
    ) {
      DELETER_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    DELETER_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    let mut data = [0u8; 1];
    {
      let isolate = &mut v8::Isolate::new(Default::default());
      let scope = std::pin::pin!(v8::HandleScope::new(isolate));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);
      let backing_store = unsafe {
        v8::ArrayBuffer::new_backing_store_from_ptr(
          data.as_mut_ptr() as *mut std::ffi::c_void,
          data.len(),
          count_deleter,
          std::ptr::null_mut(),
        )
      }
      .make_shared();
      let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store);
      assert!(buffer.detach(None).unwrap());
    }

    assert_eq!(DELETER_CALLS.load(std::sync::atomic::Ordering::SeqCst), 1);
  }

  fn assert_large_array_buffers() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "const size = 2 ** 32 + 1;\n\
       const ab = new ArrayBuffer(size);\n\
       const sab = new SharedArrayBuffer(size);\n\
       const viewSize = 2 ** 31;\n\
       const view = new Uint8Array(viewSize);\n\
       ab.byteLength === size && sab.byteLength === size &&\n\
         view.length === viewSize",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert!(script.run(scope).unwrap().boolean_value(scope));
  }

  fn assert_empty_shared_backing_store_has_null_data() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source =
      v8::String::new(scope, "new Uint8Array(new SharedArrayBuffer(0))")
        .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    let view = script.run(scope).unwrap();
    let view = v8::Local::<v8::ArrayBufferView>::try_from(view).unwrap();
    let backing_store = view.get_backing_store().unwrap();

    assert_eq!(backing_store.byte_length(), 0);
    assert!(backing_store.data().is_none());
  }

  fn assert_backing_store_survives_isolate() {
    let backing_store = {
      let isolate = &mut v8::Isolate::new(Default::default());
      let scope = std::pin::pin!(v8::HandleScope::new(isolate));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);
      let buffer = v8::ArrayBuffer::new(scope, 4);
      let backing_store = buffer.get_backing_store();
      backing_store[0].set(42);
      backing_store
    };

    assert_eq!(backing_store.byte_length(), 4);
    assert_eq!(backing_store[0].get(), 42);
  }

  fn assert_global_drop_uses_owning_isolate() {
    let mut first = v8::Isolate::new(Default::default());
    let global = {
      let scope = std::pin::pin!(v8::HandleScope::new(&mut first));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);
      let value = v8::Object::new(scope);
      v8::Global::new(scope, value)
    };

    {
      let mut second = v8::Isolate::new(Default::default());
      let scope = std::pin::pin!(v8::HandleScope::new(&mut second));
      let _scope = &mut scope.init();
      drop(global);
    }
  }

  fn assert_internal_globals_are_not_enumerable() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "[\
         '__v8x_snapshot_intrinsics',\
         '__v8xTemporalTimeZone',\
         '__v8x_import_source',\
         '__v8xPostForegroundTask',\
         '__v8xAtomicsRegisterWaiter',\
         '__v8xAtomicsNotifyWaiters',\
         '__v8xAtomicsCancelWaiter'\
       ].filter(name => Object.prototype.propertyIsEnumerable.call(\
         globalThis, name)).length",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert_eq!(script.run(scope).unwrap().integer_value(scope), Some(0));
  }

  fn assert_suspended_async_capture_survives_gc() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "globalThis.observed = 0; let release;\
       (async () => {\
         let captured = 1;\
         await new Promise(resolve => {\
           release = () => {\
             release = undefined; gc(); captured = 42; resolve();\
           };\
         });\
         observed = captured;\
       })();\
       release();",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    let source = v8::String::new(scope, "observed").unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert_eq!(script.run(scope).unwrap().integer_value(scope), Some(42));
  }

  fn assert_rejected_await_capture_survives_gc() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "globalThis.rejectedCapture = 0;\
       (async () => {\
         let captured = 41;\
         try { await Promise.reject(new Error('expected')); } catch {}\
         const read = () => captured + 1;\
         gc();\
         rejectedCapture = read();\
       })();",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    let source = v8::String::new(scope, "rejectedCapture").unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert_eq!(script.run(scope).unwrap().integer_value(scope), Some(42));
  }

  fn assert_async_iterator_close_is_awaited() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "globalThis.asyncIteratorCloseResult = null;\
       const iterator = returnMethod => ({\
         [Symbol.asyncIterator]() {\
           return {\
             next() { return Promise.resolve({ value: 1, done: false }); },\
             return: returnMethod,\
           };\
         },\
       });\
       (async () => {\
         let breakClosed = false;\
         for await (const _ of iterator(async () => {\
           await 0; breakClosed = true; return {};\
         })) { break; }\
         let returnClosed = false;\
         const returnValue = await (async () => {\
           for await (const _ of iterator(async () => {\
             await 0; returnClosed = true; return {};\
           })) { return 42; }\
         })();\
         let primitiveRejected = false;\
         try {\
           for await (const _ of iterator(async () => 1)) { break; }\
         } catch (error) {\
           primitiveRejected = error instanceof TypeError;\
         }\
         let normalReturnCalled = false;\
         for await (const _ of {\
           [Symbol.asyncIterator]() {\
             return {\
               next() { return Promise.resolve({ done: true }); },\
               return() { normalReturnCalled = true; return Promise.resolve({}); },\
             };\
           },\
         }) {}\
         asyncIteratorCloseResult =\
           `${breakClosed}:${returnClosed}:${returnValue}:` +\
           `${primitiveRejected}:${normalReturnCalled}`;\
       })();",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();

    let source = v8::String::new(scope, "asyncIteratorCloseResult").unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert_eq!(
      script.run(scope).unwrap().to_rust_string_lossy(scope),
      "true:true:42:true:false",
    );
  }

  fn assert_native_promise_then_ignores_monkeypatch() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "globalThis.promiseThenResult = 0;\
       globalThis.onFulfilled = value => promiseThenResult = value + 1;\
       globalThis.onRejected = () => promiseThenResult = -1;\
       globalThis.onCaught = value => value + 2;\
       globalThis.nativePromise = Promise.resolve(41);\
       globalThis.nativeRejectedPromise = Promise.reject(40);\
       Promise.prototype.then = () => { throw new Error('poisoned'); };\
       Promise.prototype.catch = () => { throw new Error('poisoned'); };\
       nativePromise;",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    let promise = script.run(scope).unwrap();
    let promise = v8::Local::<v8::Promise>::try_from(promise).unwrap();

    let global = context.global(scope);
    let on_fulfilled_key = v8::String::new(scope, "onFulfilled").unwrap();
    let on_fulfilled = global.get(scope, on_fulfilled_key.into()).unwrap();
    let on_fulfilled =
      v8::Local::<v8::Function>::try_from(on_fulfilled).unwrap();
    let on_rejected_key = v8::String::new(scope, "onRejected").unwrap();
    let on_rejected = global.get(scope, on_rejected_key.into()).unwrap();
    let on_rejected = v8::Local::<v8::Function>::try_from(on_rejected).unwrap();

    let chained = promise.then2(scope, on_fulfilled, on_rejected).unwrap();
    scope.perform_microtask_checkpoint();
    assert_eq!(chained.state(), v8::PromiseState::Fulfilled);
    assert_eq!(chained.result(scope).integer_value(scope), Some(42));

    let rejected_key = v8::String::new(scope, "nativeRejectedPromise").unwrap();
    let rejected = global.get(scope, rejected_key.into()).unwrap();
    let rejected = v8::Local::<v8::Promise>::try_from(rejected).unwrap();
    let on_caught_key = v8::String::new(scope, "onCaught").unwrap();
    let on_caught = global.get(scope, on_caught_key.into()).unwrap();
    let on_caught = v8::Local::<v8::Function>::try_from(on_caught).unwrap();
    let caught = rejected.catch(scope, on_caught).unwrap();
    scope.perform_microtask_checkpoint();
    assert_eq!(caught.state(), v8::PromiseState::Fulfilled);
    assert_eq!(caught.result(scope).integer_value(scope), Some(42));

    let result = v8::String::new(scope, "promiseThenResult").unwrap();
    let script = v8::Script::compile(scope, result, None).unwrap();
    assert_eq!(script.run(scope).unwrap().integer_value(scope), Some(42));
  }

  fn assert_wasm_streaming_respects_explicit_microtasks() {
    thread_local! {
      static STREAM: std::cell::RefCell<Option<v8::WasmStreaming<false>>> =
        const { std::cell::RefCell::new(None) };
    }

    let isolate = &mut v8::Isolate::new(Default::default());
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    isolate.set_wasm_streaming_callback(
      |_scope: &mut v8::PinScope,
       _url: v8::Local<v8::Value>,
       stream: v8::WasmStreaming<false>| {
        STREAM
          .with(|slot| assert!(slot.borrow_mut().replace(stream).is_none()));
      },
    );

    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let global = context.global(scope);
    let result_name = v8::String::new(scope, "streamingResult").unwrap();

    let code = v8::String::new(
      scope,
      "globalThis.streamingResult = null;\n\
       WebAssembly.compileStreaming('https://example.com')\n\
         .then(module => globalThis.streamingResult = module);",
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();

    assert!(STREAM.with(|slot| slot.borrow().is_none()));
    scope.perform_microtask_checkpoint();
    let mut stream = STREAM.with(|slot| slot.borrow_mut().take().unwrap());
    assert!(global.get(scope, result_name.into()).unwrap().is_null());

    stream.on_bytes_received(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
    stream.finish();
    assert!(global.get(scope, result_name.into()).unwrap().is_null());

    scope.perform_microtask_checkpoint();
    assert!(
      global
        .get(scope, result_name.into())
        .unwrap()
        .is_wasm_module_object()
    );
  }

  fn assert_duplicate_module_requests_resolve_once() {
    thread_local! {
      static DEPENDENCY: std::cell::RefCell<Option<v8::Global<v8::Module>>> =
        const { std::cell::RefCell::new(None) };
      static RESOLVE_COUNT: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
      };
    }

    #[allow(clippy::unnecessary_wraps)]
    fn resolve_callback<'scope>(
      context: v8::Local<'scope, v8::Context>,
      specifier: v8::Local<'scope, v8::String>,
      _attributes: v8::Local<'scope, v8::FixedArray>,
      _referrer: v8::Local<'scope, v8::Module>,
    ) -> Option<v8::Local<'scope, v8::Module>> {
      v8::callback_scope!(unsafe scope, context);
      assert_eq!(specifier.to_rust_string_lossy(scope), "original");
      RESOLVE_COUNT.with(|count| count.set(count.get() + 1));
      DEPENDENCY.with(|dependency| {
        Some(v8::Local::new(scope, dependency.borrow().as_ref().unwrap()))
      })
    }

    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let dependency_source =
      v8::String::new(scope, "export default 1; export const value = 2;")
        .unwrap();
    let dependency_origin_name =
      v8::String::new(scope, "dependency.js").unwrap();
    let dependency_origin = v8::ScriptOrigin::new(
      scope,
      dependency_origin_name.into(),
      0,
      0,
      false,
      -1,
      None,
      false,
      false,
      true,
      None,
    );
    let mut dependency_source = v8::script_compiler::Source::new(
      dependency_source,
      Some(&dependency_origin),
    );
    let dependency =
      v8::script_compiler::compile_module(scope, &mut dependency_source)
        .unwrap();
    DEPENDENCY.with(|slot| {
      *slot.borrow_mut() = Some(v8::Global::new(scope, dependency));
    });

    let root_source = v8::String::new(
      scope,
      "export * from 'original'; export { default } from 'original';",
    )
    .unwrap();
    let root_origin_name = v8::String::new(scope, "root.js").unwrap();
    let root_origin = v8::ScriptOrigin::new(
      scope,
      root_origin_name.into(),
      0,
      0,
      false,
      -1,
      None,
      false,
      false,
      true,
      None,
    );
    let mut root_source =
      v8::script_compiler::Source::new(root_source, Some(&root_origin));
    let root =
      v8::script_compiler::compile_module(scope, &mut root_source).unwrap();

    RESOLVE_COUNT.with(|count| count.set(0));
    assert_eq!(root.instantiate_module(scope, resolve_callback), Some(true));
    assert_eq!(RESOLVE_COUNT.with(|count| count.get()), 1);
    DEPENDENCY.with(|slot| slot.borrow_mut().take());
  }

  fn assert_finalization_registry_token_is_weak() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let code = v8::String::new(
      scope,
      "globalThis.finalized = [];\n\
       globalThis.registry = new FinalizationRegistry(\n\
         value => finalized.push(value)\n\
       );\n\
       (() => {\n\
         const targetAndToken = {};\n\
         registry.register(targetAndToken, 'closed', targetAndToken);\n\
       })();\n\
       globalThis.keptTarget = {};\n\
       (() => {\n\
         const token = {};\n\
         registry.register(keptTarget, 'token-collected', token);\n\
       })();\n\
       (() => {\n\
         const target = {};\n\
         const token = {};\n\
         registry.register(target, 'unregistered', token);\n\
         registry.unregister(token);\n\
       })();\n\
       gc();\n\
       keptTarget = undefined;\n\
       gc();",
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();

    let check = v8::String::new(scope, "finalized.sort().join(',')").unwrap();
    let script = v8::Script::compile(scope, check, None).unwrap();
    let result = script.run(scope).unwrap();
    assert_eq!(result.to_rust_string_lossy(scope), "closed,token-collected");
  }

  fn assert_promise_hooks_follow_continuations() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let setup = v8::String::new(
      scope,
      r#"
        globalThis.events = [];
        globalThis.ids = new Map();
        globalThis.identify = (promise) => {
          if (!ids.has(promise)) ids.set(promise, `p${ids.size + 1}`);
          return ids.get(promise);
        };
        [
          (promise, parent) => events.push(
            `init ${identify(promise)}${parent ? ` from ${identify(parent)}` : ''}`
          ),
          promise => events.push(`before ${identify(promise)}`),
          promise => events.push(`after ${identify(promise)}`),
          promise => events.push(`resolve ${identify(promise)}`),
        ];
      "#,
    )
    .unwrap();
    let hooks = v8::Script::compile(scope, setup, None)
      .unwrap()
      .run(scope)
      .unwrap();
    let hooks = v8::Local::<v8::Array>::try_from(hooks).unwrap();
    let init =
      v8::Local::<v8::Function>::try_from(hooks.get_index(scope, 0).unwrap())
        .unwrap();
    let before =
      v8::Local::<v8::Function>::try_from(hooks.get_index(scope, 1).unwrap())
        .unwrap();
    let after =
      v8::Local::<v8::Function>::try_from(hooks.get_index(scope, 2).unwrap())
        .unwrap();
    let resolve =
      v8::Local::<v8::Function>::try_from(hooks.get_index(scope, 3).unwrap())
        .unwrap();
    scope.set_promise_hooks(
      Some(init),
      Some(before),
      Some(after),
      Some(resolve),
    );

    let code = v8::String::new(
      scope,
      r#"
        async function run() {
          await Promise.resolve(1);
          Promise.reject('expected').catch(() => {});
        }
        run();
      "#,
    )
    .unwrap();
    v8::Script::compile(scope, code, None)
      .unwrap()
      .run(scope)
      .unwrap();
    scope.perform_microtask_checkpoint();

    let check = v8::String::new(scope, "events.join('\\n')").unwrap();
    let actual = v8::Script::compile(scope, check, None)
      .unwrap()
      .run(scope)
      .unwrap()
      .to_rust_string_lossy(scope);
    assert_eq!(
      actual,
      [
        "init p1",
        "init p2",
        "resolve p2",
        "init p3 from p2",
        "before p3",
        "init p4",
        "resolve p4",
        "init p5 from p4",
        "resolve p1",
        "resolve p3",
        "after p3",
        "before p5",
        "resolve p5",
        "after p5",
      ]
      .join("\n")
    );
  }

  fn assert_bigint_words_preserve_u64_max() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let source = v8::String::new(scope, "0xffffffffffffffffn").unwrap();
    let value = v8::Script::compile(scope, source, None)
      .unwrap()
      .run(scope)
      .unwrap();
    let bigint = v8::Local::<v8::BigInt>::try_from(value).unwrap();
    assert_eq!(bigint.word_count(), 1);
    let mut words = [0];
    let (negative, written) = bigint.to_words_array(&mut words);
    assert!(!negative);
    assert_eq!(written.len(), 1);
    assert_eq!(words, [u64::MAX]);
  }

  fn assert_unbound_script_preserves_origin() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let resource_name = "/some/path/to/file/users-source-code.js";
    let resource_name_value = v8::String::new(scope, resource_name).unwrap();
    let origin = v8::ScriptOrigin::new(
      scope,
      resource_name_value.into(),
      0,
      0,
      false,
      -1,
      None,
      false,
      false,
      false,
      None,
    );
    let code = v8::String::new(scope, "new Error().stack").unwrap();
    let mut source = v8::script_compiler::Source::new(code, Some(&origin));
    let unbound = v8::script_compiler::compile_unbound_script(
      scope,
      &mut source,
      v8::script_compiler::CompileOptions::NoCompileOptions,
      v8::script_compiler::NoCacheReason::NoReason,
    )
    .unwrap();
    let other_resource_name = "/some/other/source.js";
    let other_resource_name_value =
      v8::String::new(scope, other_resource_name).unwrap();
    let other_origin = v8::ScriptOrigin::new(
      scope,
      other_resource_name_value.into(),
      0,
      0,
      false,
      -1,
      None,
      false,
      false,
      false,
      None,
    );
    let mut other_source =
      v8::script_compiler::Source::new(code, Some(&other_origin));
    let other_unbound = v8::script_compiler::compile_unbound_script(
      scope,
      &mut other_source,
      v8::script_compiler::CompileOptions::NoCompileOptions,
      v8::script_compiler::NoCacheReason::NoReason,
    )
    .unwrap();

    for (script, expected_name) in [
      (unbound, resource_name),
      (other_unbound, other_resource_name),
    ] {
      let script = script.bind_to_current_context(scope);
      let result = script.run(scope).unwrap().to_rust_string_lossy(scope);
      assert!(result.contains(expected_name), "unexpected stack: {result}");
    }
  }

  fn assert_temporal_support() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let code = v8::String::new(
      scope,
      "try {\n\
       const names = Object.getOwnPropertyNames(Temporal).sort().join(',');\n\
       const duration = Temporal.Duration.from('P1DT6H30M')\n\
         .toLocaleString('en-US');\n\
       const transition = Temporal.ZonedDateTime.from(\n\
         '2020-01-01T00:00:00-05:00[America/New_York]'\n\
       ).getTimeZoneTransition('next');\n\
       const descriptors = Object.getOwnPropertyDescriptors(Temporal);\n\
       const namespaceShape = [\n\
         Object.getPrototypeOf(Temporal) === Object.prototype,\n\
         Object.prototype.toString.call(Temporal),\n\
         descriptors.Duration.enumerable,\n\
         descriptors.Duration.writable,\n\
         descriptors.Duration.configurable,\n\
         descriptors[Symbol.toStringTag].enumerable,\n\
         descriptors[Symbol.toStringTag].writable,\n\
         descriptors[Symbol.toStringTag].configurable,\n\
       ].join(',');\n\
       `${names}|${duration}|${transition}|${namespaceShape}`;\n\
       } catch (error) { `ERROR:${error.stack}`; }",
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    let result = script.run(scope).unwrap();
    assert_eq!(
      result.to_rust_string_lossy(scope),
      "Duration,Instant,Now,PlainDate,PlainDateTime,PlainMonthDay,PlainTime,\
       PlainYearMonth,ZonedDateTime|1 day, 6 hr, 30 min|\
       2020-03-08T03:00:00-04:00[America/New_York]|\
       true,[object Temporal],false,true,true,false,false,true"
    );
  }

  fn assert_intl_basics() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let source = v8::String::new(
      scope,
      "const conjunction = new Intl.ListFormat('en', {\
         style: 'long', type: 'conjunction'\
       });\
       const disjunction = new Intl.ListFormat('en', {\
         style: 'short', type: 'disjunction'\
       });\
       const locale = new Intl.Locale('zh-Hant-TW', { hourCycle: 'h12' });\
       [\
         conjunction.format(new Set(['red', 'green', 'blue'])),\
         disjunction.format(['Rust', 'Go']),\
         locale.baseName, locale.language, locale.script, locale.region,\
         locale.hourCycle, locale.numeric,\
         new Intl.Locale('de-DE-1996-fonipa').variants,\
         Object.prototype.toString.call(locale),\
         Object.keys(locale).length,\
         new Intl.Collator(undefined, { numeric: true })\
           .compare('a2', 'a10'),\
         new Intl.Collator(undefined, { sensitivity: 'variant' })\
           .compare('bar', 'Bar'),\
       ].join('|')",
    )
    .unwrap();
    let script = v8::Script::compile(scope, source, None).unwrap();
    assert_eq!(
      script.run(scope).unwrap().to_rust_string_lossy(scope),
      "red, green, and blue|Rust or Go|zh-Hant-TW|zh|Hant|TW|h12|false|\
       1996-fonipa|[object Intl.Locale]|0|-1|-1",
    );
  }

  fn assert_continuation_data_survives_await() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let extras = context.get_extras_binding_object(scope);
    let extras_name = v8::String::new(scope, "__extras").unwrap();
    context
      .global(scope)
      .set(scope, extras_name.into(), extras.into())
      .unwrap();

    let code = v8::String::new(
      scope,
      "const alreadyResolved = Promise.resolve();\n\
       const inner = { name: 'inner' };\n\
       const outer = { name: 'outer' };\n\
       __extras.setContinuationPreservedEmbedderData(inner);\n\
       async function run() {\n\
         await alreadyResolved;\n\
         globalThis.observedContinuation =\n\
           __extras.getContinuationPreservedEmbedderData().name;\n\
       }\n\
       run();\n\
       __extras.setContinuationPreservedEmbedderData(outer);",
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();

    let check = v8::String::new(
      scope,
      "observedContinuation + ':' +\n\
       (__extras.getContinuationPreservedEmbedderData() === undefined)",
    )
    .unwrap();
    let script = v8::Script::compile(scope, check, None).unwrap();
    let result = script.run(scope).unwrap();
    assert_eq!(result.to_rust_string_lossy(scope), "inner:true");
  }

  fn assert_snapshot_context_data_preserves_global_identity() {
    let context_data_indices;
    let startup_data = {
      let mut creator = v8::Isolate::snapshot_creator(None, None);
      {
        let scope = std::pin::pin!(v8::HandleScope::new(&mut creator));
        let scope = &mut scope.init();
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let code = v8::String::new(
          scope,
          "globalThis.sharedSnapshotValue = {\
             marker: 1, typeErrorPrototype: TypeError.prototype,\
             payload: 'x'.repeat(64 * 1024)\
           }; sharedSnapshotValue",
        )
        .unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let shared = script.run(scope).unwrap();
        scope.set_default_context(context);
        context_data_indices = (0..32)
          .map(|_| scope.add_context_data(context, shared))
          .collect::<Vec<_>>();
      }
      creator.create_blob(v8::FunctionCodeHandling::Keep).unwrap()
    };
    assert!(
      startup_data.len() < 1024 * 1024,
      "reference-backed context data was serialized repeatedly: {} bytes",
      startup_data.len(),
    );

    {
      let params = v8::Isolate::create_params().snapshot_blob(startup_data);
      let isolate = &mut v8::Isolate::new(params);
      let scope = std::pin::pin!(v8::HandleScope::new(isolate));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);
      let code =
        v8::String::new(scope, "globalThis.sharedSnapshotValue").unwrap();
      let script = v8::Script::compile(scope, code, None).unwrap();
      let global_value = script.run(scope).unwrap();
      for context_data_index in context_data_indices {
        let restored = scope
          .get_context_data_from_snapshot_once::<v8::Value>(context_data_index)
          .unwrap();
        assert!(restored.strict_equals(global_value));
      }
      let code = v8::String::new(
        scope,
        "globalThis.sharedSnapshotValue.typeErrorPrototype ===\
         TypeError.prototype",
      )
      .unwrap();
      let script = v8::Script::compile(scope, code, None).unwrap();
      assert!(script.run(scope).unwrap().is_true());
    }
  }

  fn assert_snapshot_global_template_handlers() {
    let setter = snapshot_global_setter.map_fn_to();
    let external_references: std::borrow::Cow<
      'static,
      [v8::ExternalReference],
    > = std::borrow::Cow::Owned(vec![v8::ExternalReference {
      named_setter: setter,
    }]);
    let context_index;
    let startup_data = {
      let mut creator =
        v8::Isolate::snapshot_creator(Some(external_references.clone()), None);
      {
        let scope = std::pin::pin!(v8::HandleScope::new(&mut creator));
        let scope = &mut scope.init();
        let template = v8::ObjectTemplate::new(scope);
        template.set_named_property_handler(
          v8::NamedPropertyHandlerConfiguration::new().setter_raw(setter),
        );
        let context = v8::Context::new(
          scope,
          v8::ContextOptions {
            global_template: Some(template),
            ..Default::default()
          },
        );
        context_index = scope.add_context(context);
      }
      creator.create_blob(v8::FunctionCodeHandling::Keep).unwrap()
    };
    assert!(
      startup_data.len() < 16 * 1024,
      "an unchanged context serialized its initialized global graph: {} bytes",
      startup_data.len(),
    );

    let params = v8::Isolate::create_params()
      .external_references(external_references)
      .snapshot_blob(startup_data);
    let isolate = &mut v8::Isolate::new(params);
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::from_snapshot(
      scope,
      context_index,
      v8::ContextOptions::default(),
    )
    .unwrap();
    let scope = &mut v8::ContextScope::new(scope, context);
    let code =
      v8::String::new(scope, "globalThis.answer = 42; globalThis.seen")
        .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    assert_eq!(script.run(scope).unwrap().int32_value(scope), Some(42));
  }
}
