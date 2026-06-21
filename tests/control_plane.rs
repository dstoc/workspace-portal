use std::{
    env,
    error::Error,
    fs::{self, Permissions},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(prefix: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    env::temp_dir().join(format!(
        "workspace-portal-{prefix}-{}-{id}",
        std::process::id()
    ))
}

fn bin_path() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_workspace-portal"))
}

fn run(args: &[&str], envs: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(bin_path());
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("failed to run workspace-portal")
}

fn output_text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    text
}

fn dev_fuse_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let fuse_device = Path::new("/dev/fuse");
        fuse_device.exists()
            && fs::metadata(fuse_device)
                .map(|m| m.file_type().is_char_device())
                .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn command_in_path(command: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(command);
            candidate.exists() && candidate.is_file()
        })
    })
}

fn looks_like_mount_permission_error(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "operation not permitted",
        "permission denied",
        "failed to mount",
        "mount failed",
        "not permitted",
        "fusermount3",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

struct Fixture {
    workspace: PathBuf,
    state_home: PathBuf,
    runtime_dir: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_dir("integration");
        let workspace = root.join("workspace");
        let state_home = root.join("xdg-state");
        let runtime_dir = root.join("xdg-runtime");
        let target = root.join("target");

        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&state_home).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&target).unwrap();

        Self {
            workspace,
            state_home,
            runtime_dir,
            target,
        }
    }

    fn envs(&self) -> [(&str, &Path); 2] {
        [
            ("XDG_STATE_HOME", self.state_home.as_path()),
            ("XDG_RUNTIME_DIR", self.runtime_dir.as_path()),
        ]
    }

    fn workspace_arg(&self) -> String {
        self.workspace.display().to_string()
    }

    fn target_arg(&self) -> String {
        self.target.display().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = run(&["stop", &self.workspace_arg(), "--force"], &self.envs());
        let _ = fs::remove_dir_all(self.workspace.parent().unwrap());
    }
}

fn edit_workspace_config(fixture: &Fixture, config: &str) -> Output {
    let script = unique_dir("editor").with_extension("sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{config}\nEOF\n"),
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();

    let base_envs = fixture.envs();
    let mut envs: Vec<(&str, &Path)> = base_envs.to_vec();
    envs.push(("VISUAL", script.as_path()));
    envs.push(("EDITOR", script.as_path()));

    let output = run(&["edit", &fixture.workspace_arg()], &envs);
    let _ = fs::remove_file(script);
    output
}

#[test]
fn cli_validation_rejects_conflicting_flags() {
    let fixture = Fixture::new();

    let start = run(
        &[
            "start",
            &fixture.workspace_arg(),
            "--allow-other",
            "--no-allow-other",
        ],
        &fixture.envs(),
    );
    assert!(!start.status.success());
    assert!(output_text(&start).contains("choose either --allow-other or --no-allow-other"));

    for removed in ["add", "rm", "freeze", "thaw"] {
        let output = run(&[removed, "--help"], &fixture.envs());
        assert!(!output.status.success());
        assert!(output_text(&output).contains("unrecognized subcommand"));
    }
}

#[test]
fn start_help_lists_nosymfollow_flag() {
    let envs: [(&str, &Path); 0] = [];
    let help = run(&["start", "--help"], &envs);

    assert!(help.status.success(), "{}", output_text(&help));
    let text = output_text(&help);
    assert!(text.contains("--nosymfollow"));
    assert!(text.contains("Disable symlink traversal through the portal mount"));
}

#[test]
fn start_leaves_workspace_empty_before_entries_are_added() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping workspace-emptiness regression test on non-Linux");
        return Ok(());
    }
    if !dev_fuse_available() {
        eprintln!("skipping workspace-emptiness regression test because /dev/fuse is unavailable");
        return Ok(());
    }
    if !command_in_path("fusermount3") {
        eprintln!(
            "skipping workspace-emptiness regression test because fusermount3 is unavailable"
        );
        return Ok(());
    }

    let fixture = Fixture::new();

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );

    if !start.status.success() {
        let text = output_text(&start);
        if looks_like_mount_permission_error(&text) {
            eprintln!(
                "skipping workspace-emptiness regression test because mounting is not permitted in this environment: {text}"
            );
            return Ok(());
        }
        panic!("{text}");
    }

    let entries: Vec<_> = fs::read_dir(&fixture.workspace)?
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.is_empty(),
        "workspace should remain empty before entries are added, found: {entries:?}"
    );

    let stop = run(
        &["stop", &fixture.workspace_arg(), "--force", "--lazy"],
        &fixture.envs(),
    );
    assert!(stop.status.success(), "{}", output_text(&stop));

    Ok(())
}

#[test]
fn control_plane_lifecycle_works_with_isolated_xdg_roots() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping control-plane lifecycle test on non-Linux");
        return Ok(());
    }
    if !dev_fuse_available() {
        eprintln!("skipping control-plane lifecycle test because /dev/fuse is unavailable");
        return Ok(());
    }
    if !command_in_path("fusermount3") {
        eprintln!("skipping control-plane lifecycle test because fusermount3 is unavailable");
        return Ok(());
    }

    let probe = Fixture::new();
    let start_probe = run(&["start", &probe.workspace_arg(), "--bg"], &probe.envs());
    if !start_probe.status.success() {
        let text = output_text(&start_probe);
        if looks_like_mount_permission_error(&text) {
            eprintln!(
                "skipping control-plane lifecycle test because mounting is not permitted in this environment: {text}"
            );
            return Ok(());
        }
        panic!("unexpected failure while probing FUSE mount support:\n{text}");
    }
    let stop_probe = run(
        &["stop", &probe.workspace_arg(), "--force", "--lazy"],
        &probe.envs(),
    );
    assert!(stop_probe.status.success(), "{}", output_text(&stop_probe));

    let fixture = Fixture::new();

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));

    let add = edit_workspace_config(
        &fixture,
        &format!(
            r#"version = 1
readlink = true
immutable_segments = []

[entries.docs]
target = "{}"
mode = "rw"
"#,
            fixture.target_arg()
        ),
    );
    assert!(add.status.success(), "{}", output_text(&add));

    let status = run(
        &["status", &fixture.workspace_arg(), "--json"],
        &fixture.envs(),
    );
    assert!(status.status.success(), "{}", output_text(&status));
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(
        status_json["workspace"],
        fixture.workspace.display().to_string()
    );
    assert_eq!(status_json["entries"].as_array().unwrap().len(), 1);
    assert_eq!(status_json["entries"][0]["name"], "docs");

    let list = run(&["list"], &fixture.envs());
    assert!(list.status.success(), "{}", output_text(&list));
    let list_text = output_text(&list);
    assert!(list_text.lines().any(|line| {
        line.contains(&fixture.workspace.display().to_string())
            && line.contains("running")
            && line.trim_end().ends_with('1')
    }));

    let rm = edit_workspace_config(
        &fixture,
        r#"version = 1
readlink = true
immutable_segments = []
"#,
    );
    assert!(rm.status.success(), "{}", output_text(&rm));

    let stopped = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stopped.status.success(), "{}", output_text(&stopped));

    let list_after_stop = run(&["list"], &fixture.envs());
    assert!(
        list_after_stop.status.success(),
        "{}",
        output_text(&list_after_stop)
    );
    let list_after_stop_text = output_text(&list_after_stop);
    assert!(list_after_stop_text.contains("stopped"));
    assert!(list_after_stop_text.contains(&fixture.workspace.display().to_string()));

    Ok(())
}
