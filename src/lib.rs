//! An icp-cli sync plugin that runs a JavaScript script against the canister
//! being synced.
//!
//! The plugin exposes to the script the same capabilities a native sync plugin
//! has — calling the target canister, the sync inputs, and read-only filesystem
//! access to the manifest's `dirs` — plus Candid, principal, and encoding
//! helpers convenient for canister work. See [`engine`] for the wiring.

wit_bindgen::generate!({
    world: "sync-plugin",
    path: "sync-plugin.wit",
});

mod candid;
mod engine;
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
