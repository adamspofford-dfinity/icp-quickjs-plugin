//! An icp-cli sync plugin that runs a JavaScript script against the canister
//! being synced.
//!
//! The plugin exposes to the script the same capabilities a native sync plugin
//! has — calling the target canister, reading its metadata sections, setting its
//! environment variables, the sync inputs, and read-only filesystem access to
//! the manifest's `dirs` — plus
//! Candid, principal, and encoding helpers convenient for canister work. See
//! [`engine`] for the wiring, [`candid`] for how an argument is written, and
//! [`interface`] for calling a method by name against the types the callee
//! declares.

wit_bindgen::generate!({
    world: "sync-plugin",
    path: "sync-plugin.wit",
});

mod candid;
mod convert;
mod engine;
mod exact;
mod fs;
mod interface;
mod number;
mod principal;

#[cfg(test)]
mod testing;

struct JsPlugin;

impl Guest for JsPlugin {
    fn exec(input: SyncExecInput) -> Result<(), String> {
        engine::run(input)
    }
}

export!(JsPlugin);
