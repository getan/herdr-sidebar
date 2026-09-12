use super::*;
use std::fs;
use std::time::SystemTime;

fn unique_root(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "herdr-watch-test-{}-{}-{}",
        tag,
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("temp root");
    dir
}

#[test]
fn skip_names_are_filtered() {
    let root = unique_root("skip");
    for rel in [
        "target/debug/app",
        "node_modules/pkg/index.js",
        ".git/index",
    ] {
        let path = root.join(rel);
        assert!(path_skipped(&path, &root, None), "{rel} should be skipped");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn normal_files_pass() {
    let root = unique_root("pass");
    assert!(!path_skipped(&root.join("src/main.rs"), &root, None));
    assert!(!path_skipped(&root.join("new-file.txt"), &root, None));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn outside_repo_is_skipped() {
    let root = unique_root("outside");
    assert!(path_skipped(
        &PathBuf::from("/tmp/elsewhere/x.rs"),
        &root,
        None
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn toplevel_gitignore_is_honored() {
    let root = unique_root("ignore");
    fs::write(root.join(".gitignore"), "*.log\n").expect("gitignore");
    let gi = load_ignores(&root).expect("matcher loads");
    assert!(path_skipped(&root.join("debug.log"), &root, Some(&gi)));
    assert!(!path_skipped(&root.join("src/main.rs"), &root, Some(&gi)));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn metadata_only_events_are_irrelevant() {
    use notify::event::MetadataKind;
    assert!(relevant_kind(&EventKind::Create(
        notify::event::CreateKind::File
    )));
    assert!(!relevant_kind(&EventKind::Modify(
        notify::event::ModifyKind::Metadata(MetadataKind::Permissions)
    )));
    assert!(!relevant_kind(&EventKind::Access(
        notify::event::AccessKind::Open(notify::event::AccessMode::Read)
    )));
}
