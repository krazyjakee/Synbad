//! Replace a complete app without invalidating its sealed code resources.
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn app_root(exe: &Path) -> Option<&Path> {
    exe.ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
}

pub(crate) fn find(root: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            if entry.file_name() == "Synbad.app" {
                return Some(entry.path());
            }
            if let Some(app) = find(&entry.path()) {
                return Some(app);
            }
        }
    }
    None
}

fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, target)?;
            #[cfg(not(unix))]
            anyhow::bail!("app symlinks require Unix");
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    fs::set_permissions(dest, fs::metadata(source)?.permissions())?;
    Ok(())
}

pub(crate) fn replace(source: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("app has no parent")?;
    let staging: PathBuf = parent.join(format!(".synbad-update-{}.app", crate::nano_unique()));
    let old = parent.join(format!(".synbad-old-{}.app", crate::nano_unique()));
    if let Err(error) = copy_tree(source, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = fs::rename(dest, &old) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&staging, dest) {
        fs::rename(&old, dest).context("restore previous app after failed update")?;
        let _ = fs::remove_dir_all(staging);
        return Err(error.into());
    }
    let _ = fs::remove_dir_all(old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaces_helpers_resources_and_signature_together() {
        let root = std::env::temp_dir().join(format!("synbad-app-test-{}", crate::nano_unique()));
        let source = root.join("new/Synbad.app");
        let dest = root.join("installed/Synbad.app");
        for app in [&source, &dest] {
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::create_dir_all(app.join("Contents/_CodeSignature")).unwrap();
        }
        fs::write(
            source.join("Contents/MacOS/deskflow-client"),
            "patched-native-core",
        )
        .unwrap();
        fs::write(
            source.join("Contents/_CodeSignature/CodeResources"),
            "new-seal",
        )
        .unwrap();
        fs::write(dest.join("Contents/obsolete"), "old").unwrap();
        replace(&source, &dest).unwrap();
        assert!(find(&root).is_some());
        assert_eq!(
            fs::read_to_string(dest.join("Contents/MacOS/deskflow-client")).unwrap(),
            "patched-native-core"
        );
        assert_eq!(
            fs::read_to_string(dest.join("Contents/_CodeSignature/CodeResources")).unwrap(),
            "new-seal"
        );
        assert!(!dest.join("Contents/obsolete").exists());
        assert_eq!(
            app_root(&dest.join("Contents/MacOS/synbadd")),
            Some(dest.as_path())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
