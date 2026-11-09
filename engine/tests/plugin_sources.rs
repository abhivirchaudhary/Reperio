//! Plug-ins dropped in as source code are built, and files that cannot be
//! used say why. One test, because it sets the plug-in folder through the
//! environment for the whole process.

use std::path::Path;
use std::process::Command;

use reperio_engine::plugin;

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

#[test]
fn source_plugins_are_built_and_problems_are_explained() {
    let root = std::env::temp_dir().join(format!("reperio-plugin-sources-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let folder = root.join("plugins");
    std::fs::create_dir_all(&folder).unwrap();
    // Builds go under $HOME/.local/share/reperio (or Library/Application Support).
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("REPERIO_PLUGINS", &folder);

    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/examples");
    let mut expected = Vec::new();
    for (file, tool, name) in [
        ("toyfs.rs", "rustc", "toyfs-rust"),
        ("toyfs.c", "cc", "toyfs-c"),
        ("toyfs.cpp", "c++", "toyfs-cpp"),
    ] {
        if have(tool) {
            std::fs::copy(examples.join(file), folder.join(file)).unwrap();
            expected.push(name);
        }
    }
    std::fs::write(folder.join("broken.rs"), "fn main() { this is broken }\n").unwrap();
    std::fs::write(folder.join("notes"), "just some notes\n").unwrap();
    std::fs::write(folder.join("README.md"), "# ignored\n").unwrap();

    let found = plugin::discovery();
    for name in &expected {
        let p = found
            .plugins
            .iter()
            .find(|p| p.name == *name)
            .unwrap_or_else(|| panic!("{name} was not built; problems: {:?}", found.problems));
        assert!(p.source.is_some(), "{name} should remember its source file");
        assert_eq!(p.fs, ["unknown"]);
    }

    let broken = found.problems.iter().find(|p| p.file == "broken.rs");
    if have("rustc") {
        let reason = &broken
            .expect("a source that does not build is reported")
            .reason;
        assert!(
            reason.contains("did not build") && reason.contains("error"),
            "{reason}"
        );
    }
    let notes = found
        .problems
        .iter()
        .find(|p| p.file == "notes")
        .expect("a plain file is reported");
    assert!(
        notes.reason.contains("not an executable"),
        "{}",
        notes.reason
    );
    assert!(
        found.problems.iter().all(|p| p.file != "README.md"),
        "documentation files are not problems"
    );

    // A second look reuses the programs already built: their files are not touched.
    let builds: Vec<_> = std::fs::read_dir(root.join("home/.local/share/reperio/plugin-builds"))
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_none())
                .collect()
        })
        .unwrap_or_default();
    let before: Vec<_> = builds
        .iter()
        .map(|e| e.metadata().unwrap().modified().unwrap())
        .collect();
    std::thread::sleep(std::time::Duration::from_secs(5)); // past the short discovery cache
    let again = plugin::discovery();
    assert_eq!(again.plugins.len(), found.plugins.len());
    let after: Vec<_> = builds
        .iter()
        .map(|e| e.metadata().unwrap().modified().unwrap())
        .collect();
    assert_eq!(before, after, "built programs are reused, not rebuilt");

    let _ = std::fs::remove_dir_all(&root);
}
