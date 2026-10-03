//! Engine selection happens at build time via cargo features.

#[cfg(all(feature = "webkit", feature = "servo"))]
compile_error!(
    "enable exactly one engine: `--features webkit` (default) OR `--no-default-features --features servo`"
);

#[cfg(not(any(feature = "webkit", feature = "servo")))]
compile_error!("enable an engine feature: `webkit` (default) or `servo`");

#[cfg(feature = "webkit")]
mod webkit;
#[cfg(feature = "webkit")]
pub use webkit::{run, NAME};

#[cfg(feature = "servo")]
mod servo_engine;
#[cfg(feature = "servo")]
pub use servo_engine::{run, NAME};
