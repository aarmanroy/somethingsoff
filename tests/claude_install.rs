//! Integration tests for `somethingsoff claude install`.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn somethingsoff(project_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(project_dir)
        .env("SOMETHINGSOFF_BASE_DIR", project_dir.join(".somethingsoff"));
    cmd
}

#[test]
fn test_project_install_writes_skill_and_gitignore() {
    let temp = TempDir::new().unwrap();
    // Simulate a git repo so the gitignore convenience kicks in.
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();

    let output = somethingsoff(temp.path())
        .args(["claude", "install"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["command"], "claude");
    assert_eq!(parsed["data"]["scope"], "project");
    assert_eq!(parsed["data"]["updated"], true);

    // Skill written with valid frontmatter.
    let skill =
        fs::read_to_string(temp.path().join(".claude/skills/somethingsoff/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\n"));
    assert!(skill.contains("name: somethingsoff"));

    // .gitignore extended, existing content preserved.
    let gitignore = fs::read_to_string(temp.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("node_modules/"));
    assert!(gitignore.contains(".somethingsoff/"));
}

#[test]
fn test_reinstall_is_idempotent() {
    let temp = TempDir::new().unwrap();

    somethingsoff(temp.path())
        .args(["claude", "install"])
        .assert()
        .success();

    // Second run: no rewrite, updated=false, and .gitignore isn't duplicated.
    let output = somethingsoff(temp.path())
        .args(["claude", "install"])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(parsed["data"]["updated"], false);
}

#[test]
fn test_outdated_skill_gets_updated() {
    let temp = TempDir::new().unwrap();
    let skill_dir = temp.path().join(".claude/skills/somethingsoff");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), "old contents").unwrap();

    let output = somethingsoff(temp.path())
        .args(["claude", "install"])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(parsed["data"]["updated"], true);

    let skill = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    assert!(skill.contains("name: somethingsoff"));
}
