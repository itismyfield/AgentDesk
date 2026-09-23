//! Windows directory aliases work under an ordinary account. File aliases
//! remain optional when Developer Mode / symlink privilege is unavailable.
use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

pub(super) fn create_directory_junction(source: &Path, link: &Path) -> io::Result<()> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SystemRoot is not set"))?;
    let powershell = Path::new(&system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    // Paths are data in separate environment variables, never interpolated
    // into shell source. Windows PowerShell is also used by our task installer.
    let output = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $target=[System.Management.Automation.WildcardPattern]::Escape($env:AGENTDESK_RUNTIME_TARGET); Set-Location -LiteralPath ([IO.Path]::GetDirectoryName($env:AGENTDESK_RUNTIME_LINK)); New-Item -ItemType Junction -Path . -Name ([IO.Path]::GetFileName($env:AGENTDESK_RUNTIME_LINK)) -Target $target | Out-Null"])
        .env("AGENTDESK_RUNTIME_LINK", std::path::absolute(link)?)
        .env("AGENTDESK_RUNTIME_TARGET", std::path::absolute(source)?)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Directory junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub(super) fn create_optional_file_alias(source: &Path, link: &Path) -> io::Result<bool> {
    match std::os::windows::fs::symlink_file(source, link) {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(1314) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn windows_junction_paths_are_literal_and_removal_preserves_target() {
        let root =
            std::env::temp_dir().join(format!("agentdesk-link [qa] & '{}'", std::process::id()));
        let source = root.join("source [qa] & '한글'");
        let link = root.join("alias [qa] & '한글'");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("owned.txt"), "preserve").unwrap();
        create_directory_junction(&source, &link).unwrap();
        assert_eq!(
            fs::read_to_string(link.join("owned.txt")).unwrap(),
            "preserve"
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir(&link).unwrap();
        assert_eq!(
            fs::read_to_string(source.join("owned.txt")).unwrap(),
            "preserve"
        );
        fs::remove_file(source.join("owned.txt")).unwrap();
        fs::remove_dir(source).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn windows_optional_file_alias_never_copies_or_hardlinks_content() {
        let root =
            std::env::temp_dir().join(format!("agentdesk-file-alias-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("canonical.md");
        let link = root.join("legacy.md");
        fs::write(&source, "first").unwrap();
        if create_optional_file_alias(&source, &link).unwrap() {
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            fs::remove_file(&link).unwrap();
        } else {
            assert!(!link.exists());
        }
        assert_eq!(fs::read_to_string(&source).unwrap(), "first");
        fs::remove_file(source).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
