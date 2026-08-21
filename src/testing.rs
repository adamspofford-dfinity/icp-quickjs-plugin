//! Helpers for the modules' tests. Most of the scripting API is only
//! observable through a running script, so the tests run one.

use crate::{FieldInput, SyncExecInput};

/// A step whose only input is `script`, declared as a field.
pub fn input(script: &str) -> SyncExecInput {
    SyncExecInput {
        canister_id: "ryjl3-tyaaa-aaaaa-aaaba-cai".to_string(),
        environment: "local".to_string(),
        dirs: vec![],
        files: vec![],
        fields: vec![FieldInput {
            name: "script".to_string(),
            value: script.to_string(),
        }],
        identity_principal: "aaaaa-aa".to_string(),
        proxy_canister_id: None,
        canister_ids: vec![],
    }
}

/// Run a script with the whole scripting API wired in.
pub fn eval(script: &str) -> Result<(), String> {
    crate::engine::run(input(script))
}

/// The error a failing script reports, for the cases that must not succeed.
pub fn error(script: &str) -> String {
    eval(script).expect_err("script was expected to fail")
}

/// Turn `[description, condition]` pairs into a script that throws the
/// description of the first condition that does not hold, so a failure names
/// what was wrong rather than just where.
pub fn assertions(checks: &[(&str, &str)]) -> String {
    checks
        .iter()
        .map(|(what, condition)| format!("if (!({condition})) throw \"{what}\";\n"))
        .collect()
}

/// Run those assertions as a script.
pub fn assert_script(checks: &[(&str, &str)]) {
    eval(&assertions(checks)).unwrap();
}
