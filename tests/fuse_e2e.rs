use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions, Permissions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};

#[cfg(unix)]
use serde_json::Value;

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

fn assert_permission_denied_operation(err: &std::io::Error) {
    assert!(
        err.kind() == ErrorKind::PermissionDenied
            || matches!(err.raw_os_error(), Some(code) if code == libc::EPERM),
        "expected permission denied operation, got {err:?}"
    );
}

fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    predicate()
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

fn looks_like_nosymfollow_unsupported_error(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let rejected_by_direct_mount = text.contains("nosymfollow")
        && (text.contains("invalid argument") || text.contains("einval"));
    let rejected_by_fusermount = text.contains("fusermount3")
        && text.contains("mount failed")
        && text.contains("permission denied");
    let rejected_by_fusermount_option_parser = text.contains("fusermount3")
        && text.contains("unknown option")
        && text.contains("nosymfollow");
    rejected_by_direct_mount || rejected_by_fusermount || rejected_by_fusermount_option_parser
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    state_home: PathBuf,
    runtime_dir: PathBuf,
    docs_target: PathBuf,
    notes_target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_dir("fuse-e2e");
        let workspace = root.join("workspace");
        let state_home = root.join("xdg-state");
        let runtime_dir = root.join("xdg-runtime");
        let docs_target = root.join("docs-target");
        let notes_target = root.join("notes-target");

        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&state_home).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&docs_target).unwrap();
        fs::create_dir_all(&notes_target).unwrap();

        Self {
            root,
            workspace,
            state_home,
            runtime_dir,
            docs_target,
            notes_target,
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = run(
            &["stop", &self.workspace_arg(), "--force", "--lazy"],
            &self.envs(),
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn status_json(fixture: &Fixture) -> Value {
    let status = run(
        &["status", &fixture.workspace_arg(), "--json"],
        &fixture.envs(),
    );
    assert!(status.status.success(), "{}", output_text(&status));
    serde_json::from_slice(&status.stdout).expect("valid status json")
}

fn wait_for_mounted_state(fixture: &Fixture, expected: bool) {
    assert!(
        wait_for(Duration::from_secs(10), || {
            status_json(fixture)["mounted"].as_bool() == Some(expected)
        }),
        "workspace mounted state never became {expected}"
    );
}

fn wait_for_mounted_file_contents(path: &Path, expected: &str) -> bool {
    wait_for(Duration::from_secs(5), || {
        fs::read_to_string(path)
            .map(|contents| contents == expected)
            .unwrap_or(false)
    })
}

fn set_workspace_entries(fixture: &Fixture, entries: &[(&str, &Path, &str)]) {
    let mut config = String::from("version = 1\nreadlink = true\nimmutable_segments = []\n");
    for (name, target, mode) in entries {
        config.push_str(&format!(
            "\n[entries.{}]\ntarget = \"{}\"\nmode = \"{}\"\n",
            name,
            toml_escape(&target.display().to_string()),
            mode
        ));
    }

    let script_path = std::env::temp_dir().join(format!(
        "workspace-portal-edit-set-{}.sh",
        std::process::id()
    ));
    fs::write(
        &script_path,
        format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{config}EOF\n"),
    )
    .expect("write edit script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))
            .expect("make edit script executable");
    }

    let edit = run_edit_with_editor(fixture, &script_path);
    let _ = fs::remove_file(&script_path);
    assert!(edit.status.success(), "{}", output_text(&edit));
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn start_rw_workspace(fixture: &Fixture) {
    start_workspace(fixture);
    set_workspace_entries(fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);
}

fn start_workspace(fixture: &Fixture) {
    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(fixture, true);
}

fn mounted_entry_names(fixture: &Fixture) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(&fixture.workspace)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn prerequisite_skip_reason() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("FUSE E2E requires Linux".to_owned());
    }

    if !dev_fuse_available() {
        return Some(
            "FUSE E2E requires real /dev/fuse access, but /dev/fuse is unavailable".to_owned(),
        );
    }

    if !command_in_path("fusermount3") {
        return Some(
            "FUSE E2E requires fusermount3, but it is unavailable in this environment".to_owned(),
        );
    }

    let probe = Fixture::new();
    fs::write(probe.docs_target.join("probe.txt"), "probe").unwrap();

    let start = run(&["start", &probe.workspace_arg(), "--bg"], &probe.envs());
    if start.status.success() {
        wait_for_mounted_state(&probe, true);
        let stop = run(
            &["stop", &probe.workspace_arg(), "--force", "--lazy"],
            &probe.envs(),
        );
        assert!(stop.status.success(), "{}", output_text(&stop));
        return None;
    }

    let text = output_text(&start);
    if looks_like_mount_permission_error(&text) {
        return Some(format!(
            "FUSE E2E mount failed; this usually means the Podman runtime lacks /dev/fuse access, CAP_SYS_ADMIN, supplementary group access, or required LSM permissions: {text}"
        ));
    }

    panic!("unexpected failure while probing FUSE mount support:\n{text}");
}

fn require_fuse_prerequisites() {
    if let Some(reason) = prerequisite_skip_reason() {
        panic!("{reason}");
    }
}

#[test]
#[ignore]
fn fuse_e2e_happy_path_covers_mount_read_write_remove_and_unmount() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("readme.txt"), "read-through-mount")?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);

    let mount_read = fs::read_to_string(fixture.workspace.join("docs/readme.txt"))?;
    assert_eq!(mount_read, "read-through-mount");

    fs::write(
        fixture.workspace.join("docs/written.txt"),
        "written through mount",
    )?;
    assert_eq!(
        fs::read_to_string(fixture.docs_target.join("written.txt"))?,
        "written through mount"
    );

    fs::rename(
        fixture.workspace.join("docs/written.txt"),
        fixture.workspace.join("docs/renamed.txt"),
    )?;
    assert!(!fixture.docs_target.join("written.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.docs_target.join("renamed.txt"))?,
        "written through mount"
    );

    set_workspace_entries(&fixture, &[]);

    assert!(
        wait_for(Duration::from_secs(5), || !fixture
            .workspace
            .join("docs")
            .exists()),
        "mounted entry never disappeared after removal"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    assert!(fixture.workspace.read_dir()?.next().is_none());

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_root_negative_entry_is_invalidated_after_add() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("readme.txt"), "root-negative")?;

    start_workspace(&fixture);

    let docs = fixture.workspace.join("docs");
    let missing = fs::metadata(&docs).unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);

    assert!(fs::metadata(&docs)?.is_dir());
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("docs/readme.txt"))?,
        "root-negative"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_root_positive_entry_is_invalidated_after_remove() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("readme.txt"), "root-positive")?;

    start_rw_workspace(&fixture);

    let docs = fixture.workspace.join("docs");
    assert!(fs::metadata(&docs)?.is_dir());
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("docs/readme.txt"))?,
        "root-positive"
    );

    set_workspace_entries(&fixture, &[]);

    let removed = fs::metadata(&docs).unwrap_err();
    assert_eq!(removed.kind(), ErrorKind::NotFound);

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_symlinks_cover_traversal_and_broken_targets() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.docs_target.join("nested"))?;
    fs::write(
        fixture.docs_target.join("nested/payload.txt"),
        "symlink-traversal",
    )?;
    symlink(
        "nested/payload.txt",
        fixture.docs_target.join("shortcut.txt"),
    )?;
    symlink("nested", fixture.docs_target.join("linked-dir"))?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);

    let shortcut = fixture.workspace.join("docs/shortcut.txt");
    assert!(fs::symlink_metadata(&shortcut)?.file_type().is_symlink());
    assert_eq!(fs::read_to_string(&shortcut)?, "symlink-traversal");

    let traversed_dir = fixture.workspace.join("docs/linked-dir/payload.txt");
    assert_eq!(fs::read_to_string(&traversed_dir)?, "symlink-traversal");

    fs::remove_file(fixture.docs_target.join("nested/payload.txt"))?;
    assert!(fs::symlink_metadata(&shortcut)?.file_type().is_symlink());
    let broken_read = fs::read_to_string(&shortcut).unwrap_err();
    assert_eq!(broken_read.kind(), ErrorKind::NotFound);

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_nosymfollow_keeps_symlinks_visible_but_blocks_traversal() -> Result<(), Box<dyn Error>>
{
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::create_dir_all(fixture.docs_target.join("nested"))?;
    fs::write(
        fixture.docs_target.join("nested/payload.txt"),
        "nosymfollow-traversal",
    )?;
    symlink(
        "nested/payload.txt",
        fixture.docs_target.join("shortcut.txt"),
    )?;
    symlink("nested", fixture.docs_target.join("linked-dir"))?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg", "--nosymfollow"],
        &fixture.envs(),
    );
    if !start.status.success() && looks_like_nosymfollow_unsupported_error(&output_text(&start)) {
        eprintln!(
            "skipping nosymfollow FUSE E2E because this kernel/FUSE mount path rejects the nosymfollow mount option"
        );
        return Ok(());
    }
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);

    let shortcut = fixture.workspace.join("docs/shortcut.txt");
    assert!(fs::symlink_metadata(&shortcut)?.file_type().is_symlink());
    assert_eq!(
        fs::read_link(&shortcut)?,
        PathBuf::from("nested/payload.txt")
    );
    assert!(fs::read_to_string(&shortcut).is_err());

    let linked_dir = fixture.workspace.join("docs/linked-dir");
    assert!(fs::symlink_metadata(&linked_dir)?.file_type().is_symlink());
    assert_eq!(fs::read_link(&linked_dir)?, PathBuf::from("nested"));
    assert!(fs::read_to_string(fixture.workspace.join("docs/linked-dir/payload.txt")).is_err());

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_soft_revocation_and_status_coherency_covers_multiple_active_entries()
-> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("alpha.txt"), "alpha-v1")?;
    fs::write(fixture.notes_target.join("beta.txt"), "beta-v1")?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(
        &fixture,
        &[
            ("docs", fixture.docs_target.as_path(), "rw"),
            ("notes", fixture.notes_target.as_path(), "ro"),
        ],
    );

    assert!(
        wait_for(Duration::from_secs(5), || {
            mounted_entry_names(&fixture)
                .map(|names| names == vec!["docs".to_owned(), "notes".to_owned()])
                .unwrap_or(false)
        }),
        "entries never became visible in the mounted workspace"
    );

    let initial_status = status_json(&fixture);
    assert_eq!(initial_status["mounted"].as_bool(), Some(true));
    assert_eq!(initial_status["entries"].as_array().unwrap().len(), 2);
    assert_eq!(
        mounted_entry_names(&fixture)?,
        vec!["docs".to_owned(), "notes".to_owned()]
    );

    fs::write(fixture.notes_target.join("beta.txt"), "beta-v2")?;
    assert!(
        wait_for_mounted_file_contents(&fixture.workspace.join("notes/beta.txt"), "beta-v2"),
        "notes entry never reflected beta-v2 through the mount"
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("notes/beta.txt"))?,
        "beta-v2"
    );

    let docs_mounted = fixture.workspace.join("docs/alpha.txt");
    assert!(
        wait_for_mounted_file_contents(&docs_mounted, "alpha-v1"),
        "docs entry never became readable through the mount"
    );
    let mut open_handle = OpenOptions::new().read(true).open(&docs_mounted)?;

    set_workspace_entries(&fixture, &[("notes", fixture.notes_target.as_path(), "ro")]);

    assert!(
        wait_for(Duration::from_secs(5), || mounted_entry_names(&fixture)
            .map(|names| names == vec!["notes".to_owned()])
            .unwrap_or(false)),
        "removed entry never disappeared from the mounted workspace"
    );
    assert!(
        wait_for(Duration::from_secs(5), || {
            fs::metadata(&docs_mounted).is_err()
        }),
        "removed entry remained visible to new lookups"
    );

    let after_rm_status = status_json(&fixture);
    assert_eq!(after_rm_status["mounted"].as_bool(), Some(true));
    assert_eq!(after_rm_status["entries"].as_array().unwrap().len(), 1);
    assert_eq!(mounted_entry_names(&fixture)?, vec!["notes".to_owned()]);

    let mut removed_entry_contents = String::new();
    open_handle.read_to_string(&mut removed_entry_contents)?;
    assert_eq!(removed_entry_contents, "alpha-v1");
    drop(open_handle);

    fs::write(fixture.notes_target.join("beta.txt"), "beta-v3")?;
    assert!(
        wait_for_mounted_file_contents(&fixture.workspace.join("notes/beta.txt"), "beta-v3"),
        "remaining notes entry never reflected beta-v3 after revocation"
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("notes/beta.txt"))?,
        "beta-v3"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);
    assert_eq!(status_json(&fixture)["mounted"].as_bool(), Some(false));

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_restart_after_stop_covers_state_recovery_and_remounting() -> Result<(), Box<dyn Error>>
{
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("recovery.txt"), "recovery-state")?;

    start_workspace(&fixture);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);
    assert_eq!(mounted_entry_names(&fixture)?, vec!["docs".to_owned()]);
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("docs/recovery.txt"))?,
        "recovery-state"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);
    assert_eq!(status_json(&fixture)["mounted"].as_bool(), Some(false));

    start_workspace(&fixture);
    assert_eq!(status_json(&fixture)["mounted"].as_bool(), Some(true));
    assert_eq!(mounted_entry_names(&fixture)?, vec!["docs".to_owned()]);
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("docs/recovery.txt"))?,
        "recovery-state"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_directory_lifecycle_covers_nested_mkdir_rmdir_and_traversal()
-> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let projects = fixture.workspace.join("docs/projects");
    let alpha = projects.join("alpha");
    let notes = alpha.join("notes");

    fs::create_dir(&projects)?;
    fs::create_dir(&alpha)?;
    fs::create_dir(&notes)?;
    fs::write(notes.join("summary.txt"), "directory lifecycle")?;

    assert_eq!(
        fs::read_to_string(fixture.docs_target.join("projects/alpha/notes/summary.txt"))?,
        "directory lifecycle"
    );
    assert_eq!(
        fs::read_to_string(
            fixture
                .workspace
                .join("docs/projects/alpha/notes/summary.txt")
        )?,
        "directory lifecycle"
    );

    let non_empty_rmdir = fs::remove_dir(&alpha);
    assert!(
        non_empty_rmdir.is_err(),
        "rmdir unexpectedly succeeded on a non-empty directory"
    );

    fs::remove_file(notes.join("summary.txt"))?;
    fs::remove_dir(&notes)?;
    fs::remove_dir(&alpha)?;
    fs::remove_dir(&projects)?;

    assert!(!fixture.docs_target.join("projects").exists());
    assert!(!fixture.workspace.join("docs/projects").exists());

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_file_lifecycle_covers_create_append_overwrite_truncate_and_fsync()
-> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted = fixture.workspace.join("docs/lifecycle.txt");
    let host = fixture.docs_target.join("lifecycle.txt");

    {
        let mut file = File::create(&mounted)?;
        file.write_all(b"alpha")?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&host)?, "alpha");
    assert_eq!(fs::read_to_string(&mounted)?, "alpha");

    {
        let mut file = OpenOptions::new().append(true).open(&mounted)?;
        file.write_all(b"-beta")?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&host)?, "alpha-beta");

    {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&mounted)?;
        file.write_all(b"gamma")?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&mounted)?, "gamma");
    assert_eq!(fs::read_to_string(&host)?, "gamma");

    {
        let file = OpenOptions::new().write(true).open(&mounted)?;
        file.set_len(0)?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&mounted)?, "");
    assert_eq!(fs::read_to_string(&host)?, "");

    {
        let mut file = File::create(&mounted)?;
        file.write_all(b"delta!")?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&mounted)?, "delta!");

    {
        let file = OpenOptions::new().write(true).open(&mounted)?;
        file.set_len(3)?;
        file.sync_all()?;
    }
    assert_eq!(fs::read_to_string(&mounted)?, "del");
    assert_eq!(fs::read_to_string(&host)?, "del");

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_touch_create_allows_noop_owner_setattr() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted = fixture.workspace.join("docs/touched.txt");
    let host = fixture.docs_target.join("touched.txt");

    let touch = Command::new("touch").arg(&mounted).output()?;
    assert!(
        touch.status.success(),
        "touch through rw entry failed: {}",
        String::from_utf8_lossy(&touch.stderr)
    );
    assert!(
        mounted.exists(),
        "touched file should remain visible in mount"
    );
    assert!(host.exists(), "touched file should exist in backing target");

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_flush_does_not_require_fsync() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted = fixture.workspace.join("docs/flush-no-fsync.txt");
    let host = fixture.docs_target.join("flush-no-fsync.txt");

    {
        let mut file = File::create(&mounted)?;
        file.write_all(b"hello-no-fsync")?;
        // No sync_all / sync_data / flush — drop triggers FUSE flush+release.
    }

    assert_eq!(fs::read_to_string(&mounted)?, "hello-no-fsync");
    assert_eq!(fs::read_to_string(&host)?, "hello-no-fsync");

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_hard_link_and_copy_cover_mutable_to_mutable_semantics() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted_source = fixture.workspace.join("docs/source.bin");
    let mounted_link = fixture.workspace.join("docs/source-link.bin");
    let mounted_copy = fixture.workspace.join("docs/source-copy.bin");
    let host_link = fixture.docs_target.join("source-host-link.bin");
    let host_link_mounted = fixture.workspace.join("docs/source-host-link.bin");

    fs::write(&mounted_source, "alpha-beta-gamma")?;

    fs::hard_link(fixture.docs_target.join("source.bin"), &host_link)?;
    assert_eq!(fs::read_to_string(&host_link_mounted)?, "alpha-beta-gamma");

    fs::hard_link(&mounted_source, &mounted_link)?;
    assert_eq!(fs::read_to_string(&mounted_link)?, "alpha-beta-gamma");
    assert_eq!(fs::read_to_string(&mounted_source)?, "alpha-beta-gamma");

    fs::write(&mounted_link, "updated-through-link")?;
    assert_eq!(fs::read_to_string(&mounted_source)?, "updated-through-link");
    assert_eq!(
        fs::read_to_string(&host_link_mounted)?,
        "updated-through-link"
    );
    assert_eq!(
        fs::read_to_string(fixture.docs_target.join("source.bin"))?,
        "updated-through-link"
    );

    fs::copy(&mounted_source, &mounted_copy)?;
    assert_eq!(fs::read_to_string(&mounted_copy)?, "updated-through-link");
    assert_eq!(
        fs::read_to_string(fixture.docs_target.join("source-copy.bin"))?,
        "updated-through-link"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_hard_link_rejects_immutable_source_and_destination() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    fs::create_dir_all(fixture.docs_target.join(".git"))?;
    fs::create_dir_all(fixture.docs_target.join(".jj"))?;
    fs::write(
        fixture.docs_target.join(".git/source.bin"),
        "immutable-source",
    )?;
    fs::write(
        fixture.docs_target.join("mutable-source.bin"),
        "mutable-source",
    )?;

    let script_path = std::env::temp_dir().join(format!(
        "workspace-portal-edit-hardlink-{}.sh",
        std::process::id()
    ));
    fs::write(
        &script_path,
        b"#!/bin/sh\nsed -i -e 's/^immutable_segments = \\[\\]$/immutable_segments = [\".git\", \".jj\"]/' \"$1\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))?;
    }

    let edited = run_edit_with_editor(&fixture, &script_path);
    assert!(edited.status.success(), "{}", output_text(&edited));

    let immutable_source = fixture.workspace.join("docs/.git/source.bin");
    let mutable_destination = fixture.workspace.join("docs/mutable-link.bin");
    let mutable_source = fixture.workspace.join("docs/mutable-source.bin");
    let immutable_destination = fixture.workspace.join("docs/.jj/mutable-link.bin");

    let source_err = fs::hard_link(&immutable_source, &mutable_destination).unwrap_err();
    assert_permission_denied_operation(&source_err);
    assert!(!mutable_destination.exists());

    let destination_err = fs::hard_link(&mutable_source, &immutable_destination).unwrap_err();
    assert_permission_denied_operation(&destination_err);
    assert!(!immutable_destination.exists());

    let _ = fs::remove_file(&script_path);
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_rename_destination_is_immediately_openable() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted_dir = fixture.workspace.join("docs/target/fs-race");
    fs::create_dir_all(&mounted_dir)?;

    for i in 1..=1000 {
        let tmp_dir = mounted_dir.join(format!("rmeta{i:04}"));
        let full = tmp_dir.join("full.rmeta");
        let destination = mounted_dir.join(format!("libtest-{i}.rmeta"));

        fs::create_dir(&tmp_dir)?;
        fs::write(&full, "abc")?;
        fs::rename(&full, &destination)?;

        let immediate_read = fs::read_to_string(&destination);
        if let Err(err) = immediate_read {
            eprintln!("failed at iteration {i}: {err}");
            eprintln!("directory listing after failure:");
            for entry in fs::read_dir(&mounted_dir)? {
                eprintln!("  {}", entry?.path().display());
            }
            let _ = fs::metadata(&destination).map(|meta| {
                eprintln!("destination inode={} size={}", meta.len(), meta.len());
            });
            return Err(Box::new(err));
        }

        fs::remove_file(&destination)?;
        fs::remove_dir(&tmp_dir)?;
    }

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_rejects_ro_writes_and_cross_entry_rename() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("alpha.txt"), "alpha")?;
    fs::write(fixture.notes_target.join("blocked.txt"), "blocked")?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(
        &fixture,
        &[
            ("docs", fixture.docs_target.as_path(), "rw"),
            ("notes", fixture.notes_target.as_path(), "ro"),
        ],
    );

    let ro_write = fs::write(fixture.workspace.join("notes/rejected.txt"), "nope");
    assert!(
        ro_write.is_err(),
        "write unexpectedly succeeded on a read-only entry"
    );

    let cross_entry_rename = fs::rename(
        fixture.workspace.join("docs/alpha.txt"),
        fixture.workspace.join("notes/moved.txt"),
    );
    assert!(
        cross_entry_rename.is_err(),
        "cross-entry rename unexpectedly succeeded"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_symlink_creation() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    set_workspace_entries(
        &fixture,
        &[
            ("docs", fixture.docs_target.as_path(), "rw"),
            ("notes", fixture.notes_target.as_path(), "ro"),
        ],
    );

    // Step 2 & 3: create symlink in rw entry
    let link_path = fixture.workspace.join("docs/my-link");
    symlink("./target", &link_path)?;
    assert!(fs::symlink_metadata(&link_path)?.file_type().is_symlink());
    assert_eq!(
        fs::read_link(&link_path)?,
        std::path::PathBuf::from("./target")
    );

    // The symlink must also be visible on the host
    assert!(
        fs::symlink_metadata(fixture.docs_target.join("my-link"))?
            .file_type()
            .is_symlink()
    );

    // Step 4: symlink creation in ro entry must fail
    let ro_link = fixture.workspace.join("notes/blocked-link");
    let ro_result = symlink("./x", &ro_link);
    assert!(
        ro_result.is_err(),
        "symlink creation in ro entry should have failed"
    );

    // Step 5: symlink creation at workspace root must fail
    let root_link = fixture.workspace.join("root-link");
    let root_result = symlink("./x", &root_link);
    assert!(
        root_result.is_err(),
        "symlink creation at workspace root should have failed"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_statfs_reports_backing_capacity() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let entry_path = fixture.workspace.join("docs");
    assert!(
        entry_path.exists(),
        "docs entry should be visible in the mount"
    );

    // -P = POSIX output (stable columns), -k = 1K blocks. Columns:
    // Filesystem  1024-blocks  Used  Available  Capacity  Mounted-on
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(&entry_path)
        .output()?;
    assert!(
        out.status.success(),
        "df failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let data_line = stdout.lines().nth(1).expect("df should print a data row");
    let cols: Vec<&str> = data_line.split_whitespace().collect();
    // Available is the 4th column (index 3) in POSIX `df -P` output.
    let available: u64 = cols[3].parse().expect("available blocks should be numeric");
    assert!(
        available > 0,
        "statfs should report nonzero available blocks, got df line: {data_line}"
    );

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_setattr_persists_timestamps() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    // Set up a workspace with a read-write entry ("docs") and a read-only entry ("notes").
    // The rw entry is backed by fixture.docs_target; the ro entry by fixture.notes_target.
    let fixture = Fixture::new();
    fs::write(fixture.docs_target.join("file.txt"), "content")?;
    fs::write(fixture.notes_target.join("file.txt"), "ro-content")?;

    let start = run(
        &["start", &fixture.workspace_arg(), "--bg"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(&fixture, true);

    set_workspace_entries(
        &fixture,
        &[
            ("docs", fixture.docs_target.as_path(), "rw"),
            ("notes", fixture.notes_target.as_path(), "ro"),
        ],
    );

    let mount_file = fixture.workspace.join("docs/file.txt");
    let host_file = fixture.docs_target.join("file.txt");

    // -----------------------------------------------------------------------
    // Step 2: touch -d '2020-01-01T00:00:00' sets mtime to a known past time.
    // Confirm stat through the mount AND on the host backing path both report
    // the 2020 mtime.
    // -----------------------------------------------------------------------
    let touch_2020 = std::process::Command::new("touch")
        .args(["-d", "2020-01-01T00:00:00Z", mount_file.to_str().unwrap()])
        .output()?;
    assert!(
        touch_2020.status.success(),
        "touch -d 2020 failed: {}",
        String::from_utf8_lossy(&touch_2020.stderr)
    );

    let expected_2020_mtime =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1577836800); // 2020-01-01T00:00:00 UTC

    let mount_mtime_2020 = fs::metadata(&mount_file)?.modified()?;
    assert!(
        mount_mtime_2020
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            == expected_2020_mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        "step 2: mount mtime should be 2020-01-01; got {:?}",
        mount_mtime_2020
    );
    let host_mtime_2020 = fs::metadata(&host_file)?.modified()?;
    assert!(
        host_mtime_2020
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            == expected_2020_mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        "step 2: host mtime should be 2020-01-01; got {:?}",
        host_mtime_2020
    );

    // -----------------------------------------------------------------------
    // Step 3: touch -a -d '2021-02-02' sets only atime (UTIME_OMIT for mtime).
    // Confirm atime changed and mtime did NOT change.
    // -----------------------------------------------------------------------
    let mtime_before_atime_only = fs::metadata(&mount_file)?.modified()?;

    let touch_atime = std::process::Command::new("touch")
        .args([
            "-a",
            "-d",
            "2021-02-02T00:00:00Z",
            mount_file.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        touch_atime.status.success(),
        "touch -a -d 2021 failed: {}",
        String::from_utf8_lossy(&touch_atime.stderr)
    );

    // atime should now reflect 2021-02-02
    let expected_2021_atime_secs = 1612224000u64; // 2021-02-02T00:00:00 UTC
    let mount_atime_2021 = fs::metadata(&mount_file)?.accessed()?;
    assert!(
        mount_atime_2021
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            == expected_2021_atime_secs,
        "step 3: mount atime should be 2021-02-02; got {:?}",
        mount_atime_2021
    );

    // mtime must be unchanged (UTIME_OMIT was honoured)
    let mtime_after_atime_only = fs::metadata(&mount_file)?.modified()?;
    assert_eq!(
        mtime_before_atime_only
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        mtime_after_atime_only
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 3: mtime must not change when only atime is updated (UTIME_OMIT)"
    );

    // Verify the same on the host backing file
    let host_atime_2021 = fs::metadata(&host_file)?.accessed()?;
    assert!(
        host_atime_2021
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            == expected_2021_atime_secs,
        "step 3: host atime should be 2021-02-02; got {:?}",
        host_atime_2021
    );

    // -----------------------------------------------------------------------
    // Step 4: cp -p copies mtime from source to destination.
    // Confirm dst mtime through the mount equals src mtime.
    // -----------------------------------------------------------------------
    let src_file = fixture.root.join("src-for-cp.txt");
    fs::write(&src_file, "cp-source-data")?;
    // Set src mtime to a known value via touch
    let touch_src = std::process::Command::new("touch")
        .args(["-d", "2019-06-15T12:00:00", src_file.to_str().unwrap()])
        .output()?;
    assert!(
        touch_src.status.success(),
        "touch src for cp -p failed: {}",
        String::from_utf8_lossy(&touch_src.stderr)
    );
    let src_mtime = fs::metadata(&src_file)?.modified()?;

    let mount_dst = fixture.workspace.join("docs/dst-cp.txt");
    let cp_p = std::process::Command::new("cp")
        .args([
            "-p",
            src_file.to_str().unwrap(),
            mount_dst.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        cp_p.status.success(),
        "cp -p failed: {}",
        String::from_utf8_lossy(&cp_p.stderr)
    );

    let dst_mount_mtime = fs::metadata(&mount_dst)?.modified()?;
    assert_eq!(
        src_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        dst_mount_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 4: dst mtime through mount should equal src mtime after cp -p"
    );

    let host_dst = fixture.docs_target.join("dst-cp.txt");
    let dst_host_mtime = fs::metadata(&host_dst)?.modified()?;
    assert_eq!(
        src_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        dst_host_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 4: dst mtime on host should equal src mtime after cp -p"
    );

    // -----------------------------------------------------------------------
    // Step 5: tar extraction preserves archived mtimes through the mount.
    // Build a tar containing a file with a known mtime, extract into the
    // entry, then confirm the restored file keeps its archived mtime.
    // -----------------------------------------------------------------------
    let tar_staging = fixture.root.join("tar-staging");
    fs::create_dir_all(&tar_staging)?;
    let archived_file = tar_staging.join("archived.txt");
    fs::write(&archived_file, "archived-data")?;
    let touch_archived = std::process::Command::new("touch")
        .args(["-d", "2018-03-10T08:00:00", archived_file.to_str().unwrap()])
        .output()?;
    assert!(
        touch_archived.status.success(),
        "touch archived.txt failed: {}",
        String::from_utf8_lossy(&touch_archived.stderr)
    );
    let archived_mtime = fs::metadata(&archived_file)?.modified()?;

    let tar_path = fixture.root.join("test.tar");
    let tar_create = std::process::Command::new("tar")
        .args([
            "-cf",
            tar_path.to_str().unwrap(),
            "-C",
            tar_staging.to_str().unwrap(),
            "archived.txt",
        ])
        .output()?;
    assert!(
        tar_create.status.success(),
        "tar create failed: {}",
        String::from_utf8_lossy(&tar_create.stderr)
    );

    let extract_dir = fixture.workspace.join("docs");
    let tar_extract = std::process::Command::new("tar")
        .args([
            "-xf",
            tar_path.to_str().unwrap(),
            "--no-same-owner",
            "-C",
            extract_dir.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        tar_extract.status.success(),
        "tar extract failed: {}",
        String::from_utf8_lossy(&tar_extract.stderr)
    );

    let extracted_mount = fixture.workspace.join("docs/archived.txt");
    let extracted_mount_mtime = fs::metadata(&extracted_mount)?.modified()?;
    assert_eq!(
        archived_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        extracted_mount_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 5: extracted file mtime through mount should match archived mtime"
    );

    let extracted_host = fixture.docs_target.join("archived.txt");
    let extracted_host_mtime = fs::metadata(&extracted_host)?.modified()?;
    assert_eq!(
        archived_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        extracted_host_mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 5: extracted file mtime on host should match archived mtime"
    );

    // -----------------------------------------------------------------------
    // Step 6: touch -d '2020-01-01T00:00:00' against a read-only entry must
    // fail (EROFS) and must NOT change the host file's mtime.
    // -----------------------------------------------------------------------
    let ro_mount_file = fixture.workspace.join("notes/file.txt");
    let ro_host_file = fixture.notes_target.join("file.txt");
    let ro_mtime_before = fs::metadata(&ro_host_file)?.modified()?;

    let touch_ro = std::process::Command::new("touch")
        .args([
            "-d",
            "2020-01-01T00:00:00Z",
            ro_mount_file.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        !touch_ro.status.success(),
        "step 6: touch -d on a read-only entry should have failed with EROFS but exited 0"
    );

    let ro_mtime_after = fs::metadata(&ro_host_file)?.modified()?;
    assert_eq!(
        ro_mtime_before
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        ro_mtime_after
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "step 6: host file mtime must be unchanged after rejected touch on ro entry"
    );

    // -----------------------------------------------------------------------
    // Teardown
    // -----------------------------------------------------------------------
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: run `edit` with a custom editor path.
//
// Extends fixture.envs() with VISUAL and EDITOR both set to `editor_script`.
// ---------------------------------------------------------------------------
fn run_edit_with_editor(fixture: &Fixture, editor_script: &Path) -> Output {
    let base_envs = fixture.envs();
    let mut env_vec: Vec<(&str, &Path)> = base_envs.to_vec();
    env_vec.push(("VISUAL", editor_script));
    env_vec.push(("EDITOR", editor_script));
    run(&["edit", &fixture.workspace_arg()], &env_vec)
}

#[test]
#[ignore]
fn fuse_e2e_edit_rw_to_ro_preserves_held_write_handle() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    // Create a file under the rw docs entry through the mount.
    fs::write(fixture.workspace.join("docs/held.txt"), "v1")?;

    // Open a long-lived write handle BEFORE flipping the entry to ro.
    let mut held = OpenOptions::new()
        .write(true)
        .open(fixture.workspace.join("docs/held.txt"))?;
    held.write_all(b"v1-held")?;

    // Build a sed editor script that flips the TOML mode line from rw → ro.
    let script_path = std::env::temp_dir().join(format!(
        "workspace-portal-edit-flip-{}.sh",
        std::process::id()
    ));
    fs::write(
        &script_path,
        b"#!/bin/sh\nsed -i 's/mode = \"rw\"/mode = \"ro\"/' \"$1\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))?;
    }

    // Run `workspace-portal edit` with the flip script.
    let edited = run_edit_with_editor(&fixture, &script_path);
    assert!(edited.status.success(), "{}", output_text(&edited));

    // Confirm the flip took effect: docs entry must now be "ro".
    let st = status_json(&fixture);
    let entries = st["entries"].as_array().expect("entries must be an array");
    let docs_entry = entries
        .iter()
        .find(|e| e["name"].as_str() == Some("docs"))
        .expect("docs entry must still be present");
    assert_eq!(
        docs_entry["mode"].as_str(),
        Some("ro"),
        "docs entry mode must be 'ro' after edit, status: {st}"
    );

    // The HELD fd must still accept writes (handle captured when rw, unaffected by flip).
    held.write_all(b"v2-after-flip")
        .expect("held write handle must still be writable after mode flip");
    drop(held);

    // A FRESH open for write must now be rejected (entry is ro).
    let fresh = OpenOptions::new()
        .write(true)
        .open(fixture.workspace.join("docs/held.txt"));
    let err = fresh.expect_err("a fresh write-open on a now-ro entry must fail");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied
        ),
        "fresh write-open must be rejected for access reasons, got: {err:?}"
    );

    // Clean up.
    let _ = fs::remove_file(&script_path);
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_fstat_on_open_unlinked_file_uses_held_handle() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let mounted_lock = fixture.workspace.join("docs/working_copy.lock");
    fs::write(&mounted_lock, "held-lock")?;

    let held = OpenOptions::new().read(true).open(&mounted_lock)?;
    fs::remove_file(&mounted_lock)?;
    assert_eq!(
        fs::metadata(&mounted_lock).unwrap_err().kind(),
        ErrorKind::NotFound,
        "new path lookups should fail after unlink"
    );

    // Let any lookup/open attributes expire so this probes daemon getattr with
    // the supplied file handle, matching jj's fstat-on-open-lock path.
    std::thread::sleep(Duration::from_millis(1100));

    let metadata = held
        .metadata()
        .expect("fstat on an open-but-unlinked file must succeed");
    assert_eq!(
        metadata.nlink(),
        0,
        "fstat on an open unlinked file should report st_nlink == 0"
    );

    drop(held);

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_edit_toml_buffer_updates_mode_and_immutable_segments() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    let script_path = std::env::temp_dir().join(format!(
        "workspace-portal-edit-toml-{}.sh",
        std::process::id()
    ));
    fs::write(
        &script_path,
        b"#!/bin/sh\nsed -i -e 's/mode = \"rw\"/mode = \"ro\"/' -e 's/^immutable_segments = \\[\\]$/immutable_segments = [\"vendor\"]/' \"$1\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))?;
    }

    let edited = run_edit_with_editor(&fixture, &script_path);
    assert!(edited.status.success(), "{}", output_text(&edited));

    let st = status_json(&fixture);
    let entries = st["entries"].as_array().expect("entries must be an array");
    let docs_entry = entries
        .iter()
        .find(|e| e["name"].as_str() == Some("docs"))
        .expect("docs entry must still be present");
    assert_eq!(
        docs_entry["mode"].as_str(),
        Some("ro"),
        "docs entry mode must be 'ro' after TOML edit, status: {st}"
    );
    let immutable_segments = st["immutable_segments"]
        .as_array()
        .expect("immutable_segments must be an array");
    assert!(
        immutable_segments
            .iter()
            .any(|segment| segment.as_str() == Some("vendor")),
        "immutable_segments must contain vendor after TOML edit, status: {st}"
    );

    let _ = fs::remove_file(&script_path);
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_edit_readlink_false_blocks_symlink_traversal_and_readlink() -> Result<(), Box<dyn Error>>
{
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    fs::create_dir_all(fixture.docs_target.join("nested"))?;
    fs::write(
        fixture.docs_target.join("nested/payload.txt"),
        "symlink-policy",
    )?;
    symlink(
        "nested/payload.txt",
        fixture.docs_target.join("shortcut.txt"),
    )?;

    let script_path = std::env::temp_dir().join(format!(
        "workspace-portal-edit-readlink-{}.sh",
        std::process::id()
    ));
    fs::write(
        &script_path,
        b"#!/bin/sh\nsed -i 's/^readlink = true$/readlink = false/' \"$1\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, Permissions::from_mode(0o755))?;
    }

    let edited = run_edit_with_editor(&fixture, &script_path);
    assert!(edited.status.success(), "{}", output_text(&edited));

    let st = status_json(&fixture);
    assert_eq!(
        st["readlink"].as_bool(),
        Some(false),
        "status after edit: {st}"
    );

    let shortcut = fixture.workspace.join("docs/shortcut.txt");
    assert!(
        fs::symlink_metadata(&shortcut)?.file_type().is_symlink(),
        "symlink inode should remain visible after disabling readlink"
    );

    let read_link_err = fs::read_link(&shortcut).expect_err("read_link should be rejected");
    assert_eq!(read_link_err.raw_os_error(), Some(libc::ELOOP));

    fs::metadata(&shortcut).expect_err("metadata through symlink should fail");

    let open_err = OpenOptions::new()
        .read(true)
        .open(&shortcut)
        .expect_err("opening through symlink should fail");
    let _ = open_err;

    let _ = fs::remove_file(&script_path);
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

// ---------------------------------------------------------------------------
// Backing-store TOCTOU confinement (see docs/proposals/symlink-confinement.md).
//
// The daemon must never resolve a host path outside the entry target, even if
// the backing store is mutated after an inode is cached. The reachable trigger
// is an *already-open* handle: the daemon serves it by cached inode without a
// fresh per-component lookup, so it rebuilds `entry.target.join(relative)` and
// follows whatever an intermediate component now points at.
//
// The probe is `fstat` on a held *file* handle. `getattr` (src/fs/callbacks.rs)
// ignores the file handle and re-derives the host path from the cached inode,
// then `lstat`s it. Standalone getattr replies use `ATTR_TTL = 0`, but the
// lookup/open path may leave lookup-returned attrs cached for `ENTRY_TTL`; wait
// for that short entry TTL to expire before probing. (A `readdir` probe is
// unreliable here: the kernel issues a readahead READDIR at opendir time,
// before the swap, and then serves the iterator from that pre-swap cache.)
// Contents cannot leak through the held fd — it was opened pre-swap against the
// real in-entry file — so this asserts on the *metadata* the daemon serves: the
// size of a file that lives OUTSIDE the entry must never be reported through the
// entry.
//
// EXPECTED: fails against current code (getattr follows the swapped `sub` and
// returns the outside file's size) and passes once daemon-side resolution is
// confined beneath the entry root.
// ---------------------------------------------------------------------------
#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_backing_store_swap_stays_confined_to_entry() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    // A directory OUTSIDE the entry target (a sibling of docs_target, so the
    // test works within a single namespace). It holds a file with the same
    // basename as the in-entry probe but a deliberately distinct size, so that
    // metadata served from inside vs. outside the entry is unambiguous.
    const OUTSIDE_CONTENTS: &str = "LEAKED-FROM-OUTSIDE-THE-ENTRY-TARGET";
    let outside = fixture.root.join("outside-secret");
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("probe"), OUTSIDE_CONTENTS)?;

    // A real subdirectory inside the entry, containing the probe file.
    let host_sub = fixture.docs_target.join("sub");
    fs::create_dir_all(&host_sub)?;
    fs::write(host_sub.join("probe"), "in")?; // 2 bytes, != OUTSIDE_CONTENTS.len()

    // Open and HOLD a file handle to docs/sub/probe through the mount. This
    // makes the daemon cache the inode for `/docs/sub/probe`; the held fd keeps
    // the kernel from forgetting it.
    let mounted_probe = fixture.workspace.join("docs/sub/probe");
    let held = OpenOptions::new().read(true).open(&mounted_probe)?;

    // TOCTOU: after the handle is open, replace the backing `sub` directory with
    // a symlink that escapes the entry target.
    fs::remove_dir_all(&host_sub)?;
    symlink(&outside, &host_sub)?;

    // `open` may have populated lookup-returned attributes with ENTRY_TTL (1s);
    // wait past that so the fstat probe exercises daemon getattr instead of a
    // kernel-cached attribute reply.
    std::thread::sleep(Duration::from_millis(1100));

    // fstat the held handle. On vulnerable code the daemon re-derives
    // `docs_target/"sub"/"probe"`, follows the swapped `sub` symlink, and
    // returns `outside/probe`'s metadata. A confined daemon either errors
    // (EXDEV) or still reports the in-entry file — both acceptable; only the
    // outside size must never be served.
    let leaked_size = held.metadata().map(|m| m.len()).unwrap_or(0);
    assert_ne!(
        leaked_size,
        OUTSIDE_CONTENTS.len() as u64,
        "confinement breach: getattr on a held handle returned metadata for a \
         file OUTSIDE the entry target after a backing-store symlink swap \
         (reported size {leaked_size} == outside file size)"
    );

    // Release the handle before stopping; an open fd keeps the mount busy and
    // would make the (non-force) unmount fail.
    drop(held);

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

#[test]
#[ignore]
fn fuse_e2e_edit_unchanged_buffer_reports_no_changes() -> Result<(), Box<dyn Error>> {
    require_fuse_prerequisites();

    let fixture = Fixture::new();
    start_rw_workspace(&fixture);

    // Use /bin/true as the editor: it exits 0 without touching the buffer file.
    let true_path = Path::new("/bin/true");

    let edited = run_edit_with_editor(&fixture, true_path);
    assert!(edited.status.success(), "{}", output_text(&edited));

    // The command must report "no changes".
    let text = output_text(&edited);
    assert!(
        text.contains("no changes"),
        "expected 'no changes' in output, got: {text}"
    );

    // docs entry must still be present and rw.
    let st = status_json(&fixture);
    let entries = st["entries"].as_array().expect("entries must be an array");
    let docs_entry = entries
        .iter()
        .find(|e| e["name"].as_str() == Some("docs"))
        .expect("docs entry must still be present after no-op edit");
    assert_eq!(
        docs_entry["mode"].as_str(),
        Some("rw"),
        "docs entry mode must remain 'rw' after no-op edit"
    );

    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: start a workspace with `--allow-other` (mirrors `start_workspace`).
// ---------------------------------------------------------------------------
fn start_workspace_allow_other(fixture: &Fixture) {
    let start = run(
        &["start", &fixture.workspace_arg(), "--bg", "--allow-other"],
        &fixture.envs(),
    );
    assert!(start.status.success(), "{}", output_text(&start));
    wait_for_mounted_state(fixture, true);
}

// ---------------------------------------------------------------------------
// Verify that `default_permissions` is enforced when `--allow-other` is used.
//
// Verification steps from docs/proposals/default-permissions-with-allow-other.md:
//   1. Start with --allow-other; expose a directory with a 0600 file owned by
//      the daemon uid.
//   2. As a different uid, attempt to read the file through the mount; must fail
//      with EACCES.
//   3. As the owner, confirm the same read still succeeds.
//
// When the test cannot arrange a second uid (running as non-root), the cross-uid
// denial check is skipped and a clear message is printed; the owner-positive path
// is still exercised.
// ---------------------------------------------------------------------------
#[test]
#[ignore]
#[cfg(unix)]
fn fuse_e2e_allow_other_enforces_file_permissions() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    require_fuse_prerequisites();

    let fixture = Fixture::new();

    // Step 1: create a 0600 file in docs_target owned by the current (daemon) uid.
    let secret_path = fixture.docs_target.join("secret.txt");
    fs::write(&secret_path, "owner-only-content")?;
    fs::set_permissions(&secret_path, Permissions::from_mode(0o600))?;

    // Start the workspace with --allow-other and add docs as a read-only entry.
    start_workspace_allow_other(&fixture);

    set_workspace_entries(&fixture, &[("docs", fixture.docs_target.as_path(), "rw")]);

    // Wait until the docs entry is visible in the mount.
    assert!(
        wait_for(Duration::from_secs(5), || fixture
            .workspace
            .join("docs")
            .exists()),
        "docs entry never appeared in the mounted workspace"
    );

    let mounted_secret = fixture.workspace.join("docs/secret.txt");

    // Step 2: cross-uid denial check — only possible when running as uid 0.
    let running_as_root = unsafe { libc::getuid() } == 0;

    if !running_as_root {
        println!(
            "skipping cross-uid denial check: not running as root \
             (uid {}); cannot drop to an unprivileged uid",
            unsafe { libc::getuid() }
        );
    } else {
        // Attempt to read the 0600 file through the mount as uid 65534 (nobody).
        // We spawn a child `cat` process with a different uid. The read must be
        // denied by the kernel (EACCES) because `default_permissions` is active.
        let deny_check = Command::new("cat")
            .arg(&mounted_secret)
            // Safety: uid() closure runs in the child between fork and exec.
            .uid(65534)
            .output()?;

        assert!(
            !deny_check.status.success(),
            "step 2: unprivileged uid 65534 read through mount should have been \
             denied (EACCES), but cat exited successfully"
        );

        let deny_stderr = String::from_utf8_lossy(&deny_check.stderr).to_ascii_lowercase();
        assert!(
            deny_stderr.contains("permission denied"),
            "step 2: expected 'permission denied' in cat stderr for unprivileged read, \
             got: {deny_stderr}"
        );
    }

    // Step 3: owner-positive path — the current uid must still be able to read
    // the 0600 file through the mount.
    let owner_read = fs::read_to_string(&mounted_secret);
    assert!(
        owner_read.is_ok(),
        "step 3: owner read through the mount should succeed, got: {:?}",
        owner_read.unwrap_err()
    );
    assert_eq!(
        owner_read.unwrap(),
        "owner-only-content",
        "step 3: owner read returned unexpected content"
    );

    // Teardown — Fixture::drop handles unmounting, but we stop explicitly so
    // the test is self-contained and failures are surfaced cleanly.
    let stop = run(&["stop", &fixture.workspace_arg()], &fixture.envs());
    assert!(stop.status.success(), "{}", output_text(&stop));
    wait_for_mounted_state(&fixture, false);

    Ok(())
}
