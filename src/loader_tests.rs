// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/loader_tests.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The command line of `serve`, option by option: every loader of main.rs is run on its own, with files in a throw-away folder. A switch that is off asks for nothing
//! else, an option without its switch is refused, a number outside its bounds is named with the bounds, a token that is too short is refused, and a bridge that is
//! told to require something it cannot check does not start. Nothing is connected: only the settings that the loaders build are looked at.

use super::*;
use std::path::PathBuf;

/// The public key of the first test vector of RFC 8032: a point that is a valid Ed25519 key.
const KEY_HEX: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const TOKEN: &str = "a-token-of-sixteen+";
const URDF: &str = r#"<robot name="arm"><link name="base"/><link name="l1"/>
  <joint name="shoulder" type="revolute"><parent link="base"/><child link="l1"/><axis xyz="0 0 1"/><limit lower="-1.0" upper="1.0" effort="1" velocity="1"/></joint>
</robot>"#;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hil-loader-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn file(dir: &std::path::Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().to_string()
}

fn err<T>(result: Result<T, String>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(message) => message,
    }
}

fn ok<T>(result: Result<T, String>) -> T {
    match result {
        Ok(value) => value,
        Err(message) => panic!("expected success, got: {message}"),
    }
}

#[test]
fn a_flag_takes_the_value_that_follows_it_unless_that_is_another_flag() {
    let a = args(&["--joint", "shoulder", "--position", "--joint", "again"]);
    assert_eq!(find_flag(&a, "--joint").as_deref(), Some("shoulder"));
    assert_eq!(
        find_flag(&a, "--position"),
        None,
        "a following flag is not a value"
    );
    assert_eq!(
        find_flag(&args(&["--last"]), "--last"),
        None,
        "nothing follows"
    );
    assert_eq!(find_flag(&a, "--absent"), None);
    assert_eq!(
        find_flag(&args(&["--n", "-5"]), "--n").as_deref(),
        Some("-5"),
        "a negative number is a value"
    );
    assert!(has_flag(&a, "--position"));
    assert!(!has_flag(&a, "--pos"), "a flag is matched whole");
}

#[test]
fn a_joint_command_needs_a_name_and_a_finite_number() {
    assert!(parse_command(&args(&["--joint", "shoulder", "--position", "0.5"])).is_ok());
    assert!(err(parse_command(&args(&["--position", "1"]))).contains("--joint"));
    assert!(err(parse_command(&args(&["--joint", "a"]))).contains("--position"));
    assert!(err(parse_command(&args(&[
        "--joint",
        "a",
        "--position",
        "fast"
    ])))
    .contains("not a valid number"));
    for bad in ["NaN", "inf", "-inf"] {
        assert!(
            parse_command(&args(&["--joint", "a", "--position", bad])).is_err(),
            "{bad}"
        );
    }
    assert!(parse_command(&args(&["--joint", " ", "--position", "1"])).is_err());
}

#[test]
fn the_interlock_starts_without_keys_and_without_believing_an_unsigned_risk() {
    let config = ok(load_interlock(&args(&[])));
    assert!(config.trusted_keys.is_empty());
    if std::env::var("HYDRA_UMC_HIL_BRIDGE_ALLOW_UNSIGNED_RISK").is_err() {
        assert!(!config.allow_unsigned_risk);
    }
    assert!(ok(load_interlock(&args(&["--allow-unsigned-risk"]))).allow_unsigned_risk);
}

#[test]
fn the_key_file_of_the_twin_is_read_checked_and_must_hold_a_key() {
    let dir = scratch("keys");
    let good = file(&dir, "keys.txt", &format!("# the twin\n\n{KEY_HEX}\n"));
    assert!(
        !ok(load_interlock(&args(&["--twin-public-key-file", &good])))
            .trusted_keys
            .is_empty()
    );
    let none = file(&dir, "none.txt", "# nothing here\n\n");
    assert!(
        err(load_interlock(&args(&["--twin-public-key-file", &none])))
            .contains("holds no public key")
    );
    let bad = file(&dir, "bad.txt", "not hexadecimal at all\n");
    let message = err(load_interlock(&args(&["--twin-public-key-file", &bad])));
    assert!(
        message.contains(&bad) && message.contains("64 hexadecimal"),
        "{message}"
    );
    let short = file(&dir, "short.txt", &KEY_HEX[..62]);
    assert!(
        err(load_interlock(&args(&["--twin-public-key-file", &short]))).contains("64 hexadecimal")
    );
    let message = err(load_interlock(&args(&[
        "--twin-public-key-file",
        &dir.join("absent").to_string_lossy(),
    ])));
    assert!(
        message.contains("cannot read the twin public key file"),
        "{message}"
    );
}

#[test]
fn the_age_of_a_certificate_is_between_one_millisecond_and_a_minute() {
    for good in ["1", "5000", "60000"] {
        assert_eq!(
            ok(load_interlock(&args(&["--certificate-max-age-ms", good])))
                .policy
                .max_age_ms,
            good.parse::<u64>().unwrap()
        );
    }
    for bad in ["0", "60001", "-1", "soon", "1.5", ""] {
        let message = err(load_interlock(&args(&["--certificate-max-age-ms", bad])));
        assert!(
            message.contains("--certificate-max-age-ms"),
            "{bad}: {message}"
        );
    }
}

#[test]
fn the_pass_of_the_twin_is_off_unless_asked_for_and_the_robot_name_is_trimmed() {
    let interlock = ok(load_interlock(&args(&[])));
    let config = ok(load_twin_pass(
        &args(&["--twin-robot-id", " arm-1 "]),
        &interlock,
    ));
    if std::env::var("HYDRA_UMC_HIL_BRIDGE_REQUIRE_TWIN_PASS").is_err() {
        assert!(!config.required && !config.locked);
    }
    assert_eq!(config.robot_id, "arm-1");
    assert!(config.file.is_none());
}

#[test]
fn requiring_the_pass_without_a_key_or_a_robot_name_does_not_start_and_with_both_it_locks() {
    let dir = scratch("pass");
    let keys = file(&dir, "keys.txt", KEY_HEX);
    let without_keys = ok(load_interlock(&args(&[])));
    let with_keys = ok(load_interlock(&args(&["--twin-public-key-file", &keys])));
    let message = err(load_twin_pass(
        &args(&["--require-twin-pass", "--twin-robot-id", "arm"]),
        &without_keys,
    ));
    assert!(
        message.contains("--twin-public-key-file") && message.contains("--twin-robot-id"),
        "{message}"
    );
    assert!(
        err(load_twin_pass(&args(&["--require-twin-pass"]), &with_keys))
            .contains("every real command would be refused")
    );
    let config = ok(load_twin_pass(
        &args(&["--require-twin-pass", "--twin-robot-id", "arm"]),
        &with_keys,
    ));
    assert!(
        config.required && config.locked,
        "required at start and not to be turned off while running"
    );
}

#[test]
fn the_file_of_the_pass_switch_is_read_when_it_is_there_and_made_later_when_it_is_not() {
    let dir = scratch("passfile");
    let interlock = ok(load_interlock(&args(&[])));
    let absent = dir.join("later.json").to_string_lossy().to_string();
    let config = ok(load_twin_pass(
        &args(&["--twin-pass-file", &absent]),
        &interlock,
    ));
    assert!(!config.required, "a file that is not there yet means off");
    assert_eq!(config.file.as_ref().unwrap().to_string_lossy(), absent);
    let on = file(&dir, "on.json", r#"{"require_twin_pass": true}"#);
    // a file that says "required" is not enough to start: without the key of the twin and the robot name every real command would be refused
    assert!(err(load_twin_pass(
        &args(&["--twin-pass-file", &on]),
        &interlock
    ))
    .contains("every real command would be refused"));
    let keys = file(&dir, "keys.txt", KEY_HEX);
    let with_keys = ok(load_interlock(&args(&["--twin-public-key-file", &keys])));
    let from_file = ok(load_twin_pass(
        &args(&["--twin-pass-file", &on, "--twin-robot-id", "arm"]),
        &with_keys,
    ));
    assert!(from_file.required, "the value of the file");
    assert!(
        !from_file.locked,
        "and it can still be changed while the bridge runs"
    );
    let off = file(
        &dir,
        "off.json",
        r#"{"require_twin_pass": false, "other": 1}"#,
    );
    assert!(
        !ok(load_twin_pass(
            &args(&["--twin-pass-file", &off]),
            &interlock
        ))
        .required
    );
    for (name, text) in [
        ("junk.json", "{not json"),
        ("shape.json", r#"{"require_twin_pass": "yes"}"#),
        ("empty.json", "{}"),
        ("list.json", "[true]"),
    ] {
        let path = file(&dir, name, text);
        let message = err(load_twin_pass(
            &args(&["--twin-pass-file", &path]),
            &interlock,
        ));
        assert!(message.contains(name), "{message}");
    }
    let message = err(load_twin_pass(
        &args(&["--twin-pass-file", &dir.to_string_lossy()]),
        &interlock,
    ));
    assert!(
        message.contains("cannot read"),
        "a folder is not a file: {message}"
    );
}

#[test]
fn real_commands_need_their_validity_window_unless_told_otherwise_and_the_longest_window_is_bounded(
) {
    if std::env::var("HYDRA_UMC_HIL_BRIDGE_ALLOW_UNDATED_COMMANDS").is_err() {
        assert!(ok(load_watchdog(&args(&[]))).require_validity);
    }
    assert!(!ok(load_watchdog(&args(&["--allow-undated-commands"]))).require_validity);
    assert_eq!(
        ok(load_watchdog(&args(&["--max-valid-for-ms", "60000"]))).max_valid_for_ms,
        60_000
    );
    assert_eq!(
        ok(load_watchdog(&args(&["--max-valid-for-ms", "1"]))).max_valid_for_ms,
        1
    );
    for bad in ["0", "60001", "later", "-3"] {
        assert!(
            err(load_watchdog(&args(&["--max-valid-for-ms", bad]))).contains("--max-valid-for-ms"),
            "{bad}"
        );
    }
}

#[test]
fn the_limits_of_the_robot_come_from_a_urdf_and_a_bad_one_is_named() {
    let dir = scratch("urdf");
    if std::env::var("HYDRA_UMC_HIL_BRIDGE_URDF").is_err() {
        assert!(ok(load_limits(&args(&[]))).is_none());
        assert!(
            ok(load_limits(&args(&["--urdf", "   "]))).is_none(),
            "a blank path is no path"
        );
    }
    let good = file(&dir, "arm.urdf", URDF);
    assert!(ok(load_limits(&args(&["--urdf", &good]))).is_some());
    let message = err(load_limits(&args(&[
        "--urdf",
        &dir.join("absent.urdf").to_string_lossy(),
    ])));
    assert!(message.contains("cannot read the URDF"), "{message}");
    let junk = file(&dir, "junk.urdf", "this is not xml");
    assert!(err(load_limits(&args(&["--urdf", &junk]))).contains(&junk));
    let other_root = file(&dir, "other.urdf", "<mjcf/>");
    assert!(err(load_limits(&args(&["--urdf", &other_root]))).contains("not <robot>"));
}

#[test]
fn the_workers_are_one_to_two_hundred_fifty_six_with_eight_by_default() {
    assert_eq!(ok(load_workers(&args(&[]))), server::DEFAULT_WORKERS);
    assert_eq!(ok(load_workers(&args(&["--workers", "1"]))), 1);
    assert_eq!(ok(load_workers(&args(&["--workers", "256"]))), 256);
    for bad in ["0", "257", "many", "-1", "2.5"] {
        assert!(
            err(load_workers(&args(&["--workers", bad]))).contains("from 1 to 256"),
            "{bad}"
        );
    }
}

#[test]
fn a_whole_number_of_milliseconds_inside_the_bounds_or_the_default() {
    assert_eq!(ok(millis_flag(&args(&[]), "--t", 500, 10, 1000)), 500);
    assert_eq!(
        ok(millis_flag(&args(&["--t", "10"]), "--t", 500, 10, 1000)),
        10
    );
    assert_eq!(
        ok(millis_flag(&args(&["--t", "1000"]), "--t", 500, 10, 1000)),
        1000
    );
    for bad in ["9", "1001", "x", "", "1e3"] {
        let message = err(millis_flag(&args(&["--t", bad]), "--t", 500, 10, 1000));
        assert!(
            message.contains("--t must be a whole number from 10 to 1000"),
            "{bad}: {message}"
        );
    }
}

#[test]
fn an_optional_token_is_a_file_or_nothing_and_must_be_long_enough() {
    let dir = scratch("token");
    let long = file(&dir, "token", &format!("  {TOKEN}\n"));
    assert_eq!(
        ok(optional_token(
            &args(&["--tok", &long]),
            "--tok",
            "HYDRA_UMC_HIL_BRIDGE_TEST_NO_SUCH_VARIABLE"
        ))
        .as_deref(),
        Some(TOKEN)
    );
    assert!(ok(optional_token(
        &args(&[]),
        "--tok",
        "HYDRA_UMC_HIL_BRIDGE_TEST_NO_SUCH_VARIABLE"
    ))
    .is_none());
    let short = file(&dir, "short", "fifteen-chars-x\n");
    assert!(err(optional_token(
        &args(&["--tok", &short]),
        "--tok",
        "HYDRA_UMC_HIL_BRIDGE_TEST_NO_SUCH_VARIABLE"
    ))
    .contains("at least 16 characters"));
    let exact = file(&dir, "exact", "0123456789abcdef");
    assert!(ok(optional_token(
        &args(&["--tok", &exact]),
        "--tok",
        "HYDRA_UMC_HIL_BRIDGE_TEST_NO_SUCH_VARIABLE"
    ))
    .is_some());
    let absent = dir.join("absent").to_string_lossy().to_string();
    assert!(err(optional_token(
        &args(&["--tok", &absent]),
        "--tok",
        "HYDRA_UMC_HIL_BRIDGE_TEST_NO_SUCH_VARIABLE"
    ))
    .contains("cannot read --tok"));
}

#[test]
fn the_link_to_the_twin_needs_a_url_a_long_token_and_a_timeout_in_range() {
    let dir = scratch("twin");
    let token = file(&dir, "token", TOKEN);
    assert!(ok(twin_endpoint(&args(&[]))).is_none());
    assert!(ok(load_twin_link(&args(&[]))).is_none());
    let (addr, read, timeout) = ok(twin_endpoint(&args(&[
        "--twin-url",
        "http://127.0.0.1:8111/",
        "--twin-state-token-file",
        &token,
    ])))
    .unwrap();
    assert_eq!(
        (addr.as_str(), read.as_str(), timeout),
        ("127.0.0.1:8111", TOKEN, 500)
    );
    assert!(ok(load_twin_link(&args(&[
        "--twin-url",
        "http://127.0.0.1:8111",
        "--twin-state-token-file",
        &token
    ])))
    .is_some());
    for bad in [
        "127.0.0.1:8111",
        "https://127.0.0.1:8111",
        "http://127.0.0.1",
        "http://",
        "http://host:1/path",
    ] {
        assert!(
            twin_endpoint(&args(&[
                "--twin-url",
                bad,
                "--twin-state-token-file",
                &token
            ]))
            .is_err(),
            "{bad}"
        );
    }
    if std::env::var("HYDRA_UMC_HIL_BRIDGE_TWIN_TOKEN").is_err() {
        assert!(err(twin_endpoint(&args(&[
            "--twin-url",
            "http://127.0.0.1:8111"
        ])))
        .contains("--twin-state-token-file"));
    }
    let short = file(&dir, "short", "tiny");
    assert!(err(twin_endpoint(&args(&[
        "--twin-url",
        "http://127.0.0.1:8111",
        "--twin-state-token-file",
        &short
    ])))
    .contains("at least 16 characters"));
    let absent = dir.join("absent").to_string_lossy().to_string();
    assert!(err(twin_endpoint(&args(&[
        "--twin-url",
        "http://127.0.0.1:8111",
        "--twin-state-token-file",
        &absent
    ])))
    .contains("cannot read"));
    for (good, expected) in [("10", 10u64), ("10000", 10_000)] {
        let (_, _, ms) = ok(twin_endpoint(&args(&[
            "--twin-url",
            "http://h:1",
            "--twin-state-token-file",
            &token,
            "--twin-timeout-ms",
            good,
        ])))
        .unwrap();
        assert_eq!(ms, expected);
    }
    for bad in ["9", "10001", "soon"] {
        assert!(
            err(twin_endpoint(&args(&[
                "--twin-url",
                "http://h:1",
                "--twin-state-token-file",
                &token,
                "--twin-timeout-ms",
                bad
            ])))
            .contains("from 10 to 10000"),
            "{bad}"
        );
    }
}

#[test]
fn the_collision_shadow_is_off_unless_asked_for_and_its_options_mean_nothing_without_it() {
    let dir = scratch("shadow");
    let token = file(&dir, "token", TOKEN);
    let twin = [
        "--twin-url",
        "http://127.0.0.1:8111",
        "--twin-state-token-file",
        token.as_str(),
    ];
    assert!(ok(load_collision_shadow(&args(&[]))).is_none());
    assert!(err(load_collision_shadow(&args(&[
        "--collision-shadow-timeout-ms",
        "100"
    ])))
    .contains("needs --collision-shadow"));
    assert!(err(load_collision_shadow(&args(&[
        "--collision-shadow-enforce"
    ])))
    .contains("needs --collision-shadow"));
    assert!(err(load_collision_shadow(&args(&["--collision-shadow"]))).contains("--twin-url"));
    let mut on = twin.to_vec();
    on.push("--collision-shadow");
    assert!(ok(load_collision_shadow(&args(&on))).is_some());
    for (good, bad) in [("10", "9"), ("10000", "10001")] {
        let mut with = on.clone();
        with.extend(["--collision-shadow-timeout-ms", good]);
        assert!(ok(load_collision_shadow(&args(&with))).is_some());
        let mut without = on.clone();
        without.extend(["--collision-shadow-timeout-ms", bad]);
        assert!(err(load_collision_shadow(&args(&without))).contains("from 10 to 10000"));
    }
}

#[test]
fn the_simulated_robot_needs_the_link_to_the_twin() {
    let dir = scratch("sim");
    let token = file(&dir, "token", TOKEN);
    assert!(ok(load_sim_robot(&args(&[]))).is_none());
    assert!(err(load_sim_robot(&args(&["--sim-robot"]))).contains("--twin-url"));
    assert!(ok(load_sim_robot(&args(&[
        "--sim-robot",
        "--twin-url",
        "http://127.0.0.1:8111",
        "--twin-state-token-file",
        &token
    ])))
    .is_some());
    assert!(
        ok(load_sim_robot(&args(&[
            "--twin-url",
            "http://127.0.0.1:8111"
        ])))
        .is_none(),
        "without the switch the link alone does nothing"
    );
}

#[test]
fn the_safety_zones_are_off_unless_given_a_url_and_their_options_need_it() {
    let dir = scratch("zones");
    let token = file(&dir, "token", TOKEN);
    assert!(ok(load_safety_zones(&args(&[]))).is_none());
    for flag in [
        "--safety-zones-token-file",
        "--safety-zones-timeout-ms",
        "--safety-zones-max-age-ms",
    ] {
        assert!(
            err(load_safety_zones(&args(&[flag, "5000"])))
                .contains(&format!("{flag} needs --safety-zones-url")),
            "{flag}"
        );
    }
    let (_, age) = ok(load_safety_zones(&args(&[
        "--safety-zones-url",
        "http://127.0.0.1:8200",
    ])))
    .unwrap();
    assert_eq!(age, 5_000, "the oldest state believed by default");
    let (_, age) = ok(load_safety_zones(&args(&[
        "--safety-zones-url",
        "http://127.0.0.1:8200",
        "--safety-zones-token-file",
        &token,
        "--safety-zones-max-age-ms",
        "100",
    ])))
    .unwrap();
    assert_eq!(age, 100);
    assert!(err(load_safety_zones(&args(&[
        "--safety-zones-url",
        "127.0.0.1:8200"
    ])))
    .contains("must start with http://"));
    for (flag, bad) in [
        ("--safety-zones-timeout-ms", "9"),
        ("--safety-zones-timeout-ms", "10001"),
        ("--safety-zones-max-age-ms", "99"),
        ("--safety-zones-max-age-ms", "60001"),
    ] {
        assert!(
            err(load_safety_zones(&args(&[
                "--safety-zones-url",
                "http://127.0.0.1:8200",
                flag,
                bad
            ])))
            .contains(flag),
            "{flag} {bad}"
        );
    }
}

#[test]
fn the_recording_is_off_unless_given_a_url_and_nothing_else_of_it_means_anything_alone() {
    let (recorder, points) = ok(load_recorder(&args(&[])));
    assert!(recorder.is_none() && points == 0);
    for flag in [
        "--record-token-file",
        "--record-timeout-ms",
        "--record-session",
        "--replay-max-points",
    ] {
        assert!(
            err(load_recorder(&args(&[flag, "5000"]))).contains("needs --record-url"),
            "{flag}"
        );
    }
    assert!(err(load_recorder(&args(&["--replay"]))).contains("--replay needs --record-url"));
}

#[test]
fn the_number_of_points_a_replay_reads_is_bounded_before_anything_is_started() {
    for bad in ["0", "200001", "all", "-1"] {
        assert!(
            err(load_recorder(&args(&[
                "--replay-max-points",
                bad,
                "--record-url",
                "http://127.0.0.1:8300"
            ])))
            .contains("from 1 to 200000"),
            "{bad}"
        );
    }
    assert!(err(load_recorder(&args(&["--record-url", "lake:8300"])))
        .contains("data lake URL must start with http://"));
    assert!(err(load_recorder(&args(&[
        "--record-url",
        "http://127.0.0.1:8300",
        "--record-timeout-ms",
        "9"
    ])))
    .contains("--record-timeout-ms"));
    assert!(err(load_recorder(&args(&[
        "--record-url",
        "http://127.0.0.1:8300",
        "--record-token-file",
        "/no/such/token/file"
    ])))
    .contains("cannot read"));
}
