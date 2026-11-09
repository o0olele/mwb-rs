//! Read-only preflight for the MSI package. Installation itself belongs to MSI.
use super::{CLIENT_EXE, SERVICE_EXE, identity};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::ErrorKind,
    os::windows::{fs::MetadataExt, io::AsRawHandle},
    path::{Path, PathBuf},
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    },
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

pub(super) fn check(client_path: &Path) -> Result<()> {
    let memory = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)? };
    let program_files = unsafe { memory.to_string() };
    unsafe { CoTaskMemFree(Some(memory.0.cast())) };
    let directory = PathBuf::from(program_files?).join("MWB-Rust");
    ensure!(
        same_path(client_path, &directory.join(CLIENT_EXE)),
        "安装位置必须为系统 Program Files 下的 MWB-Rust"
    );
    check_directory(&directory)?;
    if let Some(image) = identity::installation_service_image()? {
        ensure!(
            same_path(&image, &directory.join(SERVICE_EXE)),
            "同名输入服务属于其他目录，请先检查现有服务"
        );
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        path.components()
            .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    };
    normalize(left) == normalize(right)
}

fn check_directory(directory: &Path) -> Result<()> {
    // Reject directory junctions before MSI creates folders or applies ACLs.
    for path in directory.ancestors() {
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.is_dir()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "安装目录及上级目录不能是重解析点或普通文件：{}",
                path.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("检查安装目录"),
        }
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("读取安装目录"),
    };
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().context("安装文件名无效")?;
        ensure!(
            [CLIENT_EXE, SERVICE_EXE, "input-service.log"]
                .iter()
                .any(|allowed| name.to_string_lossy().eq_ignore_ascii_case(allowed)),
            "安装目录包含其他文件，请先检查：{}",
            path.display()
        );
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
            "安装文件不能是目录或重解析点：{}",
            path.display()
        );
        let file = fs::File::open(&path).context("读取现有安装文件的链接信息")?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)? };
        ensure!(
            info.nNumberOfLinks == 1,
            "安装文件不能是硬链接：{}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_existing_installations_are_accepted_without_changing_files() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("MWB-Rust");
        check_directory(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        for name in [CLIENT_EXE, SERVICE_EXE, "input-service.log"] {
            fs::write(directory.join(name), b"existing").unwrap();
        }
        check_directory(&directory).unwrap();
        assert_eq!(fs::read(directory.join(CLIENT_EXE)).unwrap(), b"existing");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);
        assert!(same_path(
            Path::new(r"C:\Program Files\MWB-Rust\mwb.exe"),
            Path::new(r"c:\program files\mwb-rust\mwb.exe")
        ));
        assert!(!same_path(
            Path::new(r"C:\Other\mwb.exe"),
            Path::new(r"C:\Program Files\MWB-Rust\mwb.exe")
        ));
    }

    #[test]
    fn unknown_files_and_directories_are_preserved_and_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("unrelated.txt");
        fs::write(&path, b"keep").unwrap();
        assert!(check_directory(root.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(path).unwrap();
        fs::create_dir(root.path().join(CLIENT_EXE)).unwrap();
        assert!(check_directory(root.path()).is_err());
        assert!(root.path().join(CLIENT_EXE).is_dir());
    }

    #[test]
    fn junctions_and_hard_links_cannot_redirect_installer_writes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let junction = root.path().join("MWB-Rust");
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(check_directory(&junction).is_err());
        fs::remove_dir(&junction).unwrap();
        let source = outside.path().join("keep.txt");
        fs::write(&source, b"keep").unwrap();
        fs::hard_link(&source, root.path().join(CLIENT_EXE)).unwrap();
        assert!(check_directory(root.path()).is_err());
        assert_eq!(fs::read(source).unwrap(), b"keep");
    }
}
