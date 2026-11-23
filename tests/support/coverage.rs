//! Preserve only the LLVM runtime's profile path across intentionally clean test children.

use std::ffi::OsString;

pub fn profile_env() -> Option<(&'static str, OsString)> {
    std::env::var_os("LLVM_PROFILE_FILE").map(|value| ("LLVM_PROFILE_FILE", value))
}
