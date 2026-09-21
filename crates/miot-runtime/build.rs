//! Build the runtime to wasm.
//!
//! Only under `std`: the wasm build is itself a second cargo invocation
//! targeting `wasm32`, and it must not recurse when *that* build runs.
fn main() {
    #[cfg(feature = "std")]
    substrate_wasm_builder::WasmBuilder::build_using_defaults();
}
