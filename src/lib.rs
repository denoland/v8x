//! # Example
//!
//! ```rust
//! let platform = v8::new_default_platform(0, false).make_shared();
//! v8::V8::initialize_platform(platform);
//! v8::V8::initialize();
//!
//! let isolate = &mut v8::Isolate::new(Default::default());
//!
//! let scope = std::pin::pin!(v8::HandleScope::new(isolate));
//! let scope = &mut scope.init();
//! let context = v8::Context::new(scope, Default::default());
//! let scope = &mut v8::ContextScope::new(scope, context);
//!
//! let code = v8::String::new(scope, "'Hello' + ' World!'").unwrap();
//! println!("javascript code: {}", code.to_rust_string_lossy(scope));
//!
//! let script = v8::Script::compile(scope, code, None).unwrap();
//! let result = script.run(scope).unwrap();
//! let result = result.to_string(scope).unwrap();
//! println!("result: {}", result.to_rust_string_lossy(scope));
//! ```

#![allow(clippy::missing_safety_doc)]
#![allow(unsafe_op_in_unsafe_fn)]

#[macro_use]
extern crate bitflags;
extern crate temporal_capi;

#[path = "../vendor/rusty_v8/src/array_buffer.rs"]
mod array_buffer;
#[path = "../vendor/rusty_v8/src/array_buffer_view.rs"]
mod array_buffer_view;
#[path = "../vendor/rusty_v8/src/bigint.rs"]
mod bigint;
#[path = "../vendor/rusty_v8/src/binding.rs"]
mod binding;
#[path = "../vendor/rusty_v8/src/context.rs"]
mod context;
pub use context::ContextOptions;
#[path = "../vendor/rusty_v8/src/cppgc.rs"]
pub mod cppgc;
#[path = "../vendor/rusty_v8/src/data.rs"]
mod data;
#[path = "../vendor/rusty_v8/src/date.rs"]
mod date;
#[path = "../vendor/rusty_v8/src/exception.rs"]
mod exception;
#[path = "../vendor/rusty_v8/src/external.rs"]
mod external;
#[path = "../vendor/rusty_v8/src/external_references.rs"]
mod external_references;
#[path = "../vendor/rusty_v8/src/fast_api.rs"]
pub mod fast_api;
#[path = "../vendor/rusty_v8/src/fixed_array.rs"]
mod fixed_array;
#[path = "../vendor/rusty_v8/src/function.rs"]
mod function;
#[path = "../vendor/rusty_v8/src/gc.rs"]
mod gc;
#[path = "../vendor/rusty_v8/src/get_property_names_args_builder.rs"]
mod get_property_names_args_builder;
#[path = "../vendor/rusty_v8/src/handle.rs"]
mod handle;
#[path = "../vendor/rusty_v8/src/icu.rs"]
pub mod icu;
#[path = "../vendor/rusty_v8/src/isolate.rs"]
mod isolate;
#[path = "../vendor/rusty_v8/src/isolate_group.rs"]
mod isolate_group;
#[path = "../vendor/rusty_v8/src/isolate_create_params.rs"]
mod isolate_create_params;
#[path = "../vendor/rusty_v8/src/locker.rs"]
mod locker;
#[path = "../vendor/rusty_v8/src/microtask.rs"]
mod microtask;
#[path = "../vendor/rusty_v8/src/module.rs"]
mod module;
#[path = "../vendor/rusty_v8/src/name.rs"]
mod name;
#[path = "../vendor/rusty_v8/src/number.rs"]
mod number;
#[path = "../vendor/rusty_v8/src/object.rs"]
mod object;
#[path = "../vendor/rusty_v8/src/platform.rs"]
mod platform;
#[path = "../vendor/rusty_v8/src/primitive_array.rs"]
mod primitive_array;
#[path = "../vendor/rusty_v8/src/primitives.rs"]
mod primitives;
#[path = "../vendor/rusty_v8/src/private.rs"]
mod private;
#[path = "../vendor/rusty_v8/src/promise.rs"]
mod promise;
#[path = "../vendor/rusty_v8/src/property_attribute.rs"]
mod property_attribute;
#[path = "../vendor/rusty_v8/src/property_descriptor.rs"]
mod property_descriptor;
#[path = "../vendor/rusty_v8/src/property_filter.rs"]
mod property_filter;
#[path = "../vendor/rusty_v8/src/property_handler_flags.rs"]
mod property_handler_flags;
#[path = "../vendor/rusty_v8/src/proxy.rs"]
mod proxy;
#[path = "../vendor/rusty_v8/src/regexp.rs"]
mod regexp;
#[allow(dead_code)]
#[path = "../vendor/rusty_v8/src/scope.rs"]
mod scope;
#[path = "../vendor/rusty_v8/src/script.rs"]
mod script;

#[cfg(feature = "engine_jsc")]
mod jsc;

#[cfg(feature = "engine_quickjs")]
mod quickjs;

// Pure-Rust implementation of the `crdtp__*` inspector-protocol C-ABI surface
// (engine-independent), so `test_api.rs` and friends link and run. See the
// module docs for the simplified "CBOR == JSON bytes" encoding rationale.
mod crdtp_shim;

#[path = "../vendor/rusty_v8/src/script_or_module.rs"]
mod script_or_module;
#[path = "../vendor/rusty_v8/src/shared_array_buffer.rs"]
mod shared_array_buffer;
#[path = "../vendor/rusty_v8/src/snapshot.rs"]
mod snapshot;
#[path = "../vendor/rusty_v8/src/string.rs"]
mod string;
#[path = "../vendor/rusty_v8/src/support.rs"]
mod support;
#[path = "../vendor/rusty_v8/src/symbol.rs"]
mod symbol;
#[path = "../vendor/rusty_v8/src/template.rs"]
mod template;
#[path = "../vendor/rusty_v8/src/typed_array.rs"]
mod typed_array;
#[path = "../vendor/rusty_v8/src/unbound_module_script.rs"]
mod unbound_module_script;
#[path = "../vendor/rusty_v8/src/unbound_script.rs"]
mod unbound_script;
#[path = "../vendor/rusty_v8/src/value.rs"]
mod value;
#[path = "../vendor/rusty_v8/src/value_deserializer.rs"]
mod value_deserializer;
#[path = "../vendor/rusty_v8/src/value_serializer.rs"]
mod value_serializer;
#[path = "../vendor/rusty_v8/src/wasm.rs"]
mod wasm;

#[path = "../vendor/rusty_v8/src/crdtp.rs"]
pub mod crdtp;
#[path = "../vendor/rusty_v8/src/inspector.rs"]
pub mod inspector;
#[path = "../vendor/rusty_v8/src/json.rs"]
pub mod json;
#[path = "../vendor/rusty_v8/src/script_compiler.rs"]
pub mod script_compiler;
// Unconditional since rusty_v8 v152.0.0 dropped the `simdutf` feature split
// (denoland/rusty_v8#2048): `string.rs` now calls `crate::simdutf` on every
// path. The `simdutf` Cargo feature is kept as an accepted no-op so existing
// dependency declarations keep resolving.
#[path = "../vendor/rusty_v8/src/simdutf.rs"]
pub mod simdutf;

#[allow(non_snake_case)]
#[path = "../vendor/rusty_v8/src/V8.rs"]
pub mod V8;

pub use array_buffer::*;
pub use data::*;
pub use exception::*;
pub use external_references::ExternalReference;
pub use function::*;
pub use gc::*;
pub use get_property_names_args_builder::*;
pub use handle::Eternal;
pub use handle::Global;
pub use handle::Handle;
pub use handle::Local;
pub use handle::SealedLocal;
pub use handle::TracedReference;
pub use handle::Weak;
pub use isolate::ContextDependants;
pub use isolate::GarbageCollectionType;
pub use isolate::HeapCodeStatistics;
pub use isolate::HeapSpaceStatistics;
pub use isolate::HeapStatistics;
pub use isolate::HostCreateShadowRealmContextCallback;
pub use isolate::HostImportModuleDynamicallyCallback;
pub use isolate::HostImportModuleWithPhaseDynamicallyCallback;
pub use isolate::HostInitializeImportMetaObjectCallback;
pub use isolate::IntoSharedError;
pub use isolate::IntoSharedErrorKind;
pub use isolate::Isolate;
pub use isolate::IsolateHandle;
pub use isolate::MemoryPressureLevel;
pub use isolate::MessageCallback;
pub use isolate::MessageErrorLevel;
pub use isolate::MicrotasksPolicy;
pub use isolate::ModifyCodeGenerationFromStringsCallback;
pub use isolate::ModifyCodeGenerationFromStringsResult;
pub use isolate::ModuleImportPhase;
pub use isolate::NearHeapLimitCallback;
pub use isolate::OomDetails;
pub use isolate::OomErrorCallback;
pub use isolate::OwnedIsolate;
pub use isolate::PromiseHook;
pub use isolate::PromiseHookType;
pub use isolate::PromiseRejectCallback;
pub use isolate::RealIsolate;
pub use isolate::TimeZoneDetection;
pub use isolate::UseCounterCallback;
pub use isolate::UseCounterFeature;
pub use isolate::WasmAsyncSuccess;
pub use isolate_group::IsolateGroup;
pub use isolate_create_params::CreateParams;
pub use locker::Locker;
pub use locker::SharedIsolate;
pub use microtask::MicrotaskQueue;
pub use microtask::MicrotaskQueueHandle;
pub use module::*;
pub use object::*;
pub use platform::IdleTask;
pub use platform::Platform;
pub use platform::PlatformImpl;
pub use platform::Task;
pub use platform::new_custom_platform;
pub use platform::new_default_platform;
pub use platform::new_single_threaded_default_platform;
pub use platform::new_unprotected_default_platform;
pub use primitives::*;
pub use promise::{PromiseRejectEvent, PromiseRejectMessage, PromiseState};
pub use property_attribute::*;
pub use property_descriptor::*;
pub use property_filter::*;
pub use property_handler_flags::*;
pub use regexp::RegExpCreationFlags;
pub use scope::AllowJavascriptExecutionScope;
pub use scope::allow_scopes_of_another_isolate;

pub use scope::CallbackScope;
pub use scope::ContextScope;
pub use scope::DisallowJavascriptExecutionScope;
pub use scope::EscapableHandleScope;
pub use scope::PinCallbackScope;
pub use scope::PinScope;
pub use scope::PinnedRef;
pub use scope::ScopeStorage;

pub use isolate::UnsafeRawIsolatePtr;
pub use scope::HandleScope;
pub use scope::OnFailure;
pub use scope::TryCatch;
pub use script::ScriptOrigin;
pub use script_compiler::CachedData;
pub use snapshot::FunctionCodeHandling;
pub use snapshot::StartupData;
pub use string::Encoding;
pub use string::NewStringType;
pub use string::OneByteConst;
pub use string::ValueView;
pub use string::ValueViewData;
pub use string::WriteFlags;
pub use string::WriteOptions;
pub use string::latin1_to_utf8;
pub use support::SharedPtr;
pub use support::SharedRef;
pub use support::UniquePtr;
pub use support::UniqueRef;
pub use template::*;
pub use value_deserializer::ValueDeserializer;
pub use value_deserializer::ValueDeserializerHelper;
pub use value_deserializer::ValueDeserializerImpl;
pub use value_serializer::ValueSerializer;
pub use value_serializer::ValueSerializerHelper;
pub use value_serializer::ValueSerializerImpl;
pub use wasm::CompiledWasmModule;
pub use wasm::ModuleCachingInterface;
pub use wasm::WasmModuleCompilation;
pub use wasm::WasmStreaming;

pub const MAJOR_VERSION: u32 = binding::v8__MAJOR_VERSION;

pub const MINOR_VERSION: u32 = binding::v8__MINOR_VERSION;

pub const BUILD_NUMBER: u32 = binding::v8__BUILD_NUMBER;

pub const PATCH_LEVEL: u32 = binding::v8__PATCH_LEVEL;

pub const VERSION_STRING: &str = match binding::v8__VERSION_STRING.to_str() {
  Ok(v) => v,
  Err(_) => panic!("Unable to convert CStr to &str??"),
};

pub use support::MapFnTo;

pub const TYPED_ARRAY_MAX_SIZE_IN_HEAP: usize =
  binding::v8__TYPED_ARRAY_MAX_SIZE_IN_HEAP as _;

/// The engine backend this build of the shim runs on.
pub const V8X_ENGINE: &str = if cfg!(feature = "engine_quickjs") {
  "quickjs"
} else if cfg!(feature = "vendor_jsc") {
  "jsc"
} else {
  "system-jsc"
};

#[cfg(test)]
#[allow(unused)]
pub(crate) fn initialize_v8() {
  use std::sync::Once;

  static INIT: Once = Once::new();
  INIT.call_once(|| {
    V8::initialize_platform(new_default_platform(0, false).make_shared());
    V8::initialize();
  });
}
