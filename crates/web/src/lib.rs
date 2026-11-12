#[cfg(target_arch = "wasm32")]
mod store;
#[cfg(target_arch = "wasm32")]
pub use store::Store;
