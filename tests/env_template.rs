//! Environment-variable templating, in its own test binary.
//!
//! `std::env::set_var` mutates process-global state that other threads may be reading,
//! which is why it is `unsafe` in edition 2024. Rust runs the tests inside one binary
//! concurrently, so this file deliberately contains exactly one test: its own process,
//! no other thread, no race.

use std::collections::HashMap;

#[test]
fn env_placeholders_resolve_from_the_environment() {
    // SAFETY: this binary contains a single test, so nothing else can be reading the
    // environment while it is written.
    unsafe { std::env::set_var("PILOTAGE_TEST_AVATAR", "avatar-123") };

    let vars = HashMap::new();
    assert_eq!(
        pilotage::util::template("id={env.PILOTAGE_TEST_AVATAR}", &vars),
        "id=avatar-123"
    );

    // A set variable resolves; an unset one stays visible rather than silently emptying.
    assert_eq!(
        pilotage::util::template(
            "a={env.PILOTAGE_TEST_AVATAR} b={env.PILOTAGE_UNSET_XYZ}",
            &vars
        ),
        "a=avatar-123 b={env.PILOTAGE_UNSET_XYZ}"
    );
}
