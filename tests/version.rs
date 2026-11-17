use std::process::Command;

/// The version a build of this test's own environment should report: bare
/// only when `TRIP_RELEASE=1` was set at compile time, as the release
/// installer sets it, and `-dev` for every other build. Run the test under
/// `TRIP_RELEASE=1 cargo test --test version` to check the release side.
fn expected() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match option_env!("TRIP_RELEASE") {
        Some("1") => format!("trip {version}\n"),
        _ => format!("trip {version}-dev\n"),
    }
}

#[test]
fn version_says_whether_the_build_is_a_release() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_trip"))
            .arg(flag)
            .output()
            .expect("run trip");
        assert!(out.status.success(), "trip {flag} failed: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            expected(),
            "trip {flag}"
        );
    }
}
