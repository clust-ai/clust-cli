use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_flag_exits_zero() {
    Command::cargo_bin("clust")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Agent manager CLI"));
}

#[test]
fn version_flag_exits_zero() {
    Command::cargo_bin("clust")
        .unwrap()
        .arg("--version")
        .assert()
        .success();
}

#[test]
fn ls_help_exits_zero() {
    Command::cargo_bin("clust")
        .unwrap()
        .args(["ls", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("List all running agents"));
}

#[test]
fn ui_help_exits_zero() {
    Command::cargo_bin("clust")
        .unwrap()
        .args(["ui", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("terminal UI"));
}

#[test]
fn invalid_flag_exits_nonzero() {
    Command::cargo_bin("clust")
        .unwrap()
        .arg("--nonsense")
        .assert()
        .failure();
}

#[test]
fn pool_flag_in_help_output() {
    Command::cargo_bin("clust")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--pool"));
}

#[test]
fn ls_pool_flag_in_help_output() {
    Command::cargo_bin("clust")
        .unwrap()
        .args(["ls", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--pool"));
}

#[test]
fn invalid_pool_name_exits_nonzero() {
    Command::cargo_bin("clust")
        .unwrap()
        .args(["-p", "INVALID"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid pool name"));
}

#[test]
fn invalid_pool_name_on_ls_exits_nonzero() {
    Command::cargo_bin("clust")
        .unwrap()
        .args(["ls", "-p", "BAD-name"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid pool name"));
}
