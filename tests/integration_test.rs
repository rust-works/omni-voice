#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use anyhow::Result;

use omni_voice::data::amendments::AmendmentFile;

#[test]
fn amendment_file_parsing() -> Result<()> {
    // Test that amendment file parsing works correctly
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root)?;
        tempfile::tempdir_in(&tmp_root)?
    };
    let yaml_path = temp_dir.path().join("test_amendments.yaml");

    // Create a test amendment file
    let test_yaml = r#"
amendments:
  - commit: "1234567890abcdef1234567890abcdef12345678"
    message: "Updated commit message 1"
  - commit: "abcdef1234567890abcdef1234567890abcdef12"
    message: "Updated commit message 2"
"#;

    fs::write(&yaml_path, test_yaml)?;

    // Test loading the amendment file
    let amendment_file = AmendmentFile::load_from_file(&yaml_path)?;
    assert_eq!(amendment_file.amendments.len(), 2);

    println!("✅ Amendment file parsing test passed");
    Ok(())
}

#[test]
fn amendment_validation() -> Result<()> {
    // Test amendment validation
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root)?;
        tempfile::tempdir_in(&tmp_root)?
    };
    let yaml_path = temp_dir.path().join("invalid_amendments.yaml");

    // Test with invalid commit hash (too short)
    let invalid_yaml = r#"
amendments:
  - commit: "12345"
    message: "Short hash should fail"
"#;

    fs::write(&yaml_path, invalid_yaml)?;

    // This should fail validation
    let result = AmendmentFile::load_from_file(&yaml_path);
    assert!(result.is_err());
    println!("✅ Amendment validation test passed - invalid hash rejected");

    Ok(())
}

#[test]
fn help_all_golden() -> Result<()> {
    // Capture the help-all output using the help generator directly
    use omni_voice::cli::help::HelpGenerator;

    let generator = HelpGenerator::new();
    let help_output = generator.generate_all_help()?;

    // Use insta for snapshot testing - this creates a .snap file with the expected output
    insta::assert_snapshot!("help_all_output", help_output);

    println!("✅ Golden test for help-all command completed");
    Ok(())
}

// ── CLI binary invocation tests ─────────────────────────────────

#[test]
fn binary_help_flag_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .arg("--help")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("comprehensive development toolkit"));
}

#[test]
fn binary_version_flag_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .arg("--version")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("omni-voice"));
}

#[test]
fn binary_unknown_command_fails() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .arg("nonexistent")
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
}

#[test]
fn binary_help_all_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .arg("help-all")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("omni-voice"));
    assert!(stdout.contains("voice"));
}

#[test]
fn binary_completions_bash_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["completions", "bash"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("complete -F _omni-voice"),
        "missing bash completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_zsh_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["completions", "zsh"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#compdef omni-voice"),
        "missing zsh compdef marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_fish_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["completions", "fish"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("complete -c omni-voice"),
        "missing fish completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_powershell_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["completions", "powershell"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Register-ArgumentCompleter"),
        "missing PowerShell completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_unknown_shell_fails() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["completions", "banana"])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
}

#[test]
fn binary_commands_generate_in_temp_dir() {
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root).ok();
        tempfile::tempdir_in(&tmp_root).unwrap()
    };
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-voice"))
        .args(["commands", "generate", "all"])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());

    // Verify templates were written
    assert!(temp_dir
        .path()
        .join(".claude/commands/commit-twiddle.md")
        .exists());
    assert!(temp_dir
        .path()
        .join(".claude/commands/pr-create.md")
        .exists());
    assert!(temp_dir
        .path()
        .join(".claude/commands/pr-update.md")
        .exists());
}
