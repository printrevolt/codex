use codex_shell_command::is_safe_command::is_known_safe_command;

#[test]
fn bash_full_path_lc_safe_examples() {
    assert!(is_known_safe_command(&[
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "ls -la".to_string()
    ]));
}

#[test]
fn zsh_full_path_lc_safe_examples() {
    assert!(is_known_safe_command(&[
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        "ls -la".to_string()
    ]));
}
