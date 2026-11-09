use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let resource = output.join("mwb-version.rc");
    let compiled = output.join("mwb-version.res");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = ["MAJOR", "MINOR", "PATCH"]
        .map(|part| {
            env::var(format!("CARGO_PKG_VERSION_{part}"))
                .unwrap()
                .parse::<u16>()
                .expect("Windows version components must fit in 16 bits")
                .to_string()
        })
        .join(",");
    fs::write(
        &resource,
        format!(
            r#"1 VERSIONINFO
FILEVERSION {numeric},0
PRODUCTVERSION {numeric},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "MWB Rust\0"
      VALUE "FileVersion", "{version}.0\0"
      VALUE "ProductName", "MWB Rust\0"
      VALUE "ProductVersion", "{version}\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
        ),
    )
    .expect("Write Windows version resource");
    let compiler =
        resource_compiler().expect("Windows SDK rc.exe is required to build MWB on Windows");
    let status = Command::new(compiler)
        .args(["/nologo", "/fo"])
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Run Windows SDK resource compiler");
    assert!(
        status.success(),
        "Windows version resource compilation failed"
    );
    println!("cargo:rustc-link-arg-bins={}", compiled.display());
}

fn resource_compiler() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RC").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    if let Some(path) = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join("rc.exe"))
            .find(|path| path.is_file())
    }) {
        return Some(path);
    }
    let mut sdk_roots = Vec::new();
    if let Some(path) = env::var_os("WindowsSdkDir") {
        sdk_roots.push(PathBuf::from(path));
    }
    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(path) = env::var_os(variable) {
            sdk_roots.push(PathBuf::from(path).join("Windows Kits").join("10"));
        }
    }
    let mut candidates = Vec::new();
    for sdk in sdk_roots {
        let Ok(versions) = fs::read_dir(sdk.join("bin")) else {
            continue;
        };
        for entry in versions.flatten() {
            let compiler = entry.path().join("x64").join("rc.exe");
            if compiler.is_file() {
                let version = entry
                    .file_name()
                    .to_string_lossy()
                    .split('.')
                    .filter_map(|part| part.parse::<u32>().ok())
                    .collect::<Vec<_>>();
                candidates.push((version, compiler));
            }
        }
    }
    candidates.sort();
    candidates.pop().map(|(_, compiler)| compiler)
}
