//! Platform-dependent modifier meaning comes from the host, not the WASM target.
use crate::wire::keyboard::Modifiers;
pub fn command(modifiers: Modifiers) -> bool { if crate::slots::macos() { modifiers.logo } else { modifiers.control } }
pub fn jump(modifiers: Modifiers) -> bool { if crate::slots::macos() { modifiers.alt } else { modifiers.control } }
pub fn macos_command(modifiers: Modifiers) -> bool { crate::slots::macos() && modifiers.logo }
