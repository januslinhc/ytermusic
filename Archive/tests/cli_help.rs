use assert_cmd::Command;
use predicates::str::contains;

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "the test must fail immediately if Cargo cannot locate the binary"
)]
fn help_explains_the_product_and_support_commands() {
    Command::cargo_bin("ytermusic")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Music without leaving your terminal"))
        .stdout(contains("doctor"))
        .stdout(contains("auth"));
}
