//! CLI tests. These spawn the real binary and a real detached server, but do
//! not require a terminal.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rscreen")
}

struct Env {
    _dir: tempfile::TempDir,
    runtime: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("run");
        std::fs::create_dir_all(&runtime).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[general]
shell = "/bin/sh"
shell_args = ["-c", "printf 'CLI_READY\n'; sleep 60"]
status_rows = 1
"#,
        )
        .unwrap();
        Self {
            _dir: dir,
            runtime,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.env("RSCREEN_RUNTIME_DIR", &self.runtime)
            .env("RSCREEN_CONFIG", &self.config);
        cmd
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = self.command().args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn wait_for<F: Fn() -> bool>(f: F, millis: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}

#[test]
fn create_list_kill_session() {
    let env = Env::new();
    let (ok, out, err) = env.run(&["new", "cli-test", "--detached"]);
    assert!(ok, "new failed: {}\n{}", out, err);
    assert!(out.contains("cli-test"));
    let (ok, out, err) = env.run(&["ls"]);
    assert!(ok, "{}", err);
    assert!(out.contains("cli-test"), "ls output: {}", out);
    assert!(out.contains("detached"));
    let (ok, out, err) = env.run(&["kill", "cli-test"]);
    assert!(ok, "kill failed: {}\n{}", out, err);
    assert!(out.contains("killed"));
    assert!(wait_for(
        || {
            let (_, out, _) = env.run(&["ls"]);
            out.contains("No rscreen sessions")
        },
        3000
    ));
}

#[test]
fn duplicate_session_name_is_rejected() {
    let env = Env::new();
    let (ok, _, err) = env.run(&["new", "dup", "--detached"]);
    assert!(ok, "{}", err);
    let (ok, _, err) = env.run(&["new", "dup", "--detached"]);
    assert!(!ok);
    assert!(
        err.contains("already exists") || err.contains("being created"),
        "{}",
        err
    );
    let _ = env.run(&["kill", "dup"]);
}

#[test]
fn attach_missing_session_is_a_clear_error() {
    let env = Env::new();
    let (ok, _, err) = env.run(&["attach", "does-not-exist"]);
    assert!(!ok);
    assert!(err.contains("no such session"), "{}", err);
}

#[test]
fn config_print_init_and_overwrite() {
    let env = Env::new();
    let (ok, out, err) = env.run(&["config"]);
    assert!(ok, "{}", err);
    assert!(out.contains(&env.config.display().to_string()));
    let (ok, out, _) = env.run(&["config", "--print"]);
    assert!(ok);
    assert!(out.contains("[general]"));
    assert!(out.contains("prefix"));
    // The helper config already exists.
    let (ok, _, err) = env.run(&["config", "--init"]);
    assert!(!ok);
    assert!(err.contains("already exists"), "{}", err);
    let (ok, _, err) = env.run(&["config", "--init", "--force"]);
    assert!(ok, "{}", err);
    assert!(Path::new(&env.config).exists());
}

#[test]
fn session_names_are_validated_and_private() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    let (ok, _, err) = env.run(&["new", "../escape", "--detached"]);
    assert!(!ok);
    assert!(err.contains("session name"), "{}", err);
    let (ok, _, err) = env.run(&["attach", "../escape"]);
    assert!(!ok);
    assert!(err.contains("session name"), "{}", err);
    assert!(!env._dir.path().join("escape.sock").exists());
    assert!(!Path::new("/tmp/escape.sock").exists());

    let (ok, _, err) = env.run(&["new", "secure", "--detached"]);
    assert!(ok, "{}", err);
    let runtime_mode = std::fs::metadata(&env.runtime)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let socket = env.runtime.join("secure.sock");
    let socket_mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(runtime_mode, 0o700, "runtime dir must be private");
    assert_eq!(socket_mode, 0o600, "session socket must be private");
    let _ = env.run(&["kill", "secure"]);
}

#[test]
fn insecure_runtime_path_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("actual");
    std::fs::create_dir(&target).unwrap();
    let link = dir.path().join("runtime-link");
    symlink(&target, &link).unwrap();
    let out = Command::new(bin())
        .env("RSCREEN_RUNTIME_DIR", &link)
        .args(["new", "nope", "--detached"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not a directory") || err.contains("not private"),
        "{}",
        err
    );

    let file = dir.path().join("runtime-file");
    std::fs::write(&file, b"x").unwrap();
    let out = Command::new(bin())
        .env("RSCREEN_RUNTIME_DIR", &file)
        .args(["ls"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("runtime directory"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn unsafe_config_targets_are_ignored() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    let (ok, _, err) = env.run(&["new", "safe-config", "--detached"]);
    assert!(ok, "{}", err);
    // Replace the regular config with a symlink to a device. Loading must warn
    // and fall back to defaults instead of hanging or executing it.
    std::fs::remove_file(&env.config).unwrap();
    symlink("/dev/zero", &env.config).unwrap();
    let (ok, _, err) = env.run(&["attach", "safe-config"]);
    assert!(!ok); // no tty in the test, but config loading must warn first
    assert!(
        err.contains("cannot read") || err.contains("not a regular file"),
        "{}",
        err
    );
    let _ = env.run(&["kill", "safe-config"]);

    let env2 = Env::new();
    let (ok, _, err) = env2.run(&["new", "safe-config2", "--detached"]);
    assert!(ok, "{}", err);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&env2.config, std::fs::Permissions::from_mode(0o666)).unwrap();
    let (ok, _, err) = env2.run(&["attach", "safe-config2"]);
    assert!(!ok);
    assert!(err.contains("group/world-writable"), "{}", err);
    let _ = env2.run(&["kill", "safe-config2"]);
}

#[test]
fn detached_session_survives_and_is_queryable() {
    let env = Env::new();
    let (ok, _, err) = env.run(&["new", "survivor", "--detached"]);
    assert!(ok, "{}", err);
    std::thread::sleep(Duration::from_millis(250));
    let (ok, out, err) = env.run(&["ls"]);
    assert!(ok, "{}", err);
    assert!(out.contains("survivor"));
    assert!(!out.contains("CLI_READY")); // ls should not dump pane text
    let (ok, _, err) = env.run(&["kill", "survivor"]);
    assert!(ok, "{}", err);
}
