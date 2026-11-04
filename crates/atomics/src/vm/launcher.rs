//! The native VM app launcher, installed without requiring a source checkout.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn install() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    let applications = atomics_core::paths::home().join("Applications");
    let app = install_in(&applications)?;
    println!("Installed {}", app.display());
    Ok(())
}
fn install_in(applications: &Path) -> Result<PathBuf> {
    let work = tempfile::tempdir()?;
    let script = work.path().join("launcher.sh");
    let source = work.path().join("launcher.c");
    std::fs::write(&script, include_str!("../../../../scripts/macos-vm-launcher.sh"))?;
    std::fs::write(&source, include_str!("../../../../scripts/macos-vm-launcher.c"))?;
    let output =
        Command::new("/bin/sh").arg(script).arg(applications).arg(source).output().context("installing VM launcher")?;
    if !output.status.success() {
        bail!("VM launcher: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(applications.join("ATOMICS VM.app"))
}
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn launcher_uses_the_sibling_main_binary_and_replaces_only_its_own_id() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("ATOMICS.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir(contents.join("Resources")).unwrap();
        let binary = contents.join("MacOS/atomics");
        std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(contents.join("Resources/ATOMICS.icns"), b"same-icon").unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            "<plist version=\"1.0\"><dict><key>CFBundleShortVersionString</key><string>1.2.3</string></dict></plist>",
        )
        .unwrap();
        let app = install_in(dir.path()).unwrap();
        let output =
            Command::new(app.join("Contents/MacOS/atomics-vm")).arg("-psn_123").arg("--size=800x600").output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "--connect\nvm\n--size=800x600\n");
        std::fs::write(app.join("old-handmade-file"), "old").unwrap();
        install_in(dir.path()).unwrap();
        assert!(!app.join("old-handmade-file").exists());
        let plist = app.join("Contents/Info.plist");
        std::fs::write(
            &plist,
            std::fs::read_to_string(&plist).unwrap().replace("ai.atomisk.atomics.vm", "example.other"),
        )
        .unwrap();
        assert!(install_in(dir.path()).is_err());
        assert!(std::fs::read_to_string(plist).unwrap().contains("example.other"));
    }
}
