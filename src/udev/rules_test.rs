use super::{format_hide_rules, RULE_HIDE_DEVICE_EARLY_PRIORITY, RULE_HIDE_DEVICE_LATE_PRIORITY};
use std::{fs, path::PathBuf, process::Command};

fn generated_rule_pairs() -> Vec<(String, String)> {
    let mut rules = Vec::new();
    for (name, subsystem, match_rule) in [
        ("event42", "input", r#"KERNELS=="input42""#),
        ("js42", "input", r#"KERNELS=="input42""#),
        ("hidraw42", "hidraw", r#"KERNELS=="0003:1234:5678.0042""#),
    ] {
        for permission_commands in [None, Some(("/usr/bin/chmod", "/usr/bin/setfacl"))] {
            for move_command in [None, Some("/usr/bin/mv")] {
                rules.push(format_hide_rules(
                    name,
                    subsystem,
                    match_rule,
                    permission_commands,
                    move_command,
                ));
            }
        }
    }
    rules
}

#[test]
fn generated_rules_use_only_forward_local_jumps() {
    for (early_rule, late_rule) in generated_rule_pairs() {
        for rule in [early_rule, late_rule] {
            let lines: Vec<_> = rule.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if let Some((_, jump)) = line.split_once("GOTO=\"") {
                    let target = jump.split('"').next().unwrap();
                    let label = format!("LABEL=\"{target}\"");
                    assert!(
                        lines[index + 1..].contains(&label.as_str()),
                        "GOTO target {target} must occur later in the same file:\n{rule}"
                    );
                }
            }
        }
    }
}

#[test]
fn generated_rules_preserve_two_stage_actions() {
    let (early_rule, late_rule) = format_hide_rules(
        "event42",
        "input",
        r#"KERNELS=="input42""#,
        Some(("/usr/bin/chmod", "/usr/bin/setfacl")),
        Some("/usr/bin/mv"),
    );
    assert_eq!(
        early_rule.matches(r#"MODE:="0000", GROUP:="root""#).count(),
        2
    );
    assert_eq!(
        early_rule
            .matches(r#"SYMLINK+="inputplumber/by-hidden/%k""#)
            .count(),
        2
    );
    assert!(!early_rule.contains("uaccess"));
    assert_eq!(
        late_rule
            .matches(r#"MODE="000", GROUP="root", TAG-="uaccess""#)
            .count(),
        2
    );
    assert!(!late_rule.contains("SYMLINK"));
    for rule in [early_rule, late_rule] {
        assert!(rule.contains(r#"KERNELS=="input42", GOTO="inputplumber_valid""#));
        for path in ["/dev/input/%k", "/dev/%k"] {
            for command in [
                format!("/usr/bin/chmod 000 {path}"),
                format!("/usr/bin/setfacl -b {path}"),
                format!("/usr/bin/mv {path} /dev/inputplumber/sources/%k"),
            ] {
                assert_eq!(rule.matches(&format!("RUN+=\"{command}\"")).count(), 1);
            }
        }
    }
}

struct TestRulesDirectory(PathBuf);

impl Drop for TestRulesDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires udevadm with the verify command; parses temporary files only"]
fn generated_rules_pass_udevadm_verify() {
    let path = std::env::temp_dir().join(format!("inputplumber-rules-{}", std::process::id()));
    fs::create_dir(&path).unwrap();
    let directory = TestRulesDirectory(path);
    let early_path = directory.0.join(format!(
        "{RULE_HIDE_DEVICE_EARLY_PRIORITY}-inputplumber-hide-test-early.rules"
    ));
    let late_path = directory.0.join(format!(
        "{RULE_HIDE_DEVICE_LATE_PRIORITY}-inputplumber-hide-test-late.rules"
    ));
    let verify = || {
        Command::new("udevadm")
            .args(["verify", "--no-style"])
            .arg(&early_path)
            .arg(&late_path)
            .output()
            .expect("udevadm verify must be installed to run this test")
    };
    for (early_rule, late_rule) in generated_rule_pairs() {
        fs::write(&early_path, early_rule).unwrap();
        fs::write(&late_path, late_rule).unwrap();
        let output = verify();
        assert!(
            output.status.success(),
            "udevadm verify failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Negative control: a label in the other file cannot satisfy the old jump.
    fs::write(&early_path, "GOTO=\"inputplumber_hide_test_late\"\n").unwrap();
    fs::write(&late_path, "LABEL=\"inputplumber_hide_test_late\"\n").unwrap();
    assert!(!verify().status.success());
}
