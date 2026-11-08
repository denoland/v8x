//! JavaScriptCore engine backend: the v8__* C-ABI implemented over JSC.
#![allow(non_snake_case)]

pub(crate) mod arraybuffer;
pub(crate) mod cli_extra;
pub(crate) mod core;
pub(crate) mod exception;
pub(crate) mod function;
pub(crate) mod init;
pub(crate) mod inspector;
pub(crate) mod introspect;
pub(crate) mod isolate;
pub(crate) mod jsc_sys;
pub(crate) mod locker;
pub(crate) mod misc;
pub(crate) mod module;
pub(crate) mod object;
pub(crate) mod primitive;
pub(crate) mod property;
pub(crate) mod serializer;
pub(crate) mod shims;
pub(crate) mod simdutf;
pub(crate) mod string;
pub(crate) mod terminate;
pub(crate) mod value;
