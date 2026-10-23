//! Compiles the Slint UI and, for Windows (mingw) builds, the icon +
//! version resource.

use std::path::PathBuf;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");

    println!("cargo:rerun-if-changed=assets/icon.ico");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "gnu" {
        windows_resource();
    }
}

/// `windres` an .rc with the icon (id 1: Explorer, taskbar, tray) and a
/// VERSIONINFO, linked into the exe as a COFF object.
fn windows_resource() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut nums: Vec<u16> = version.split(['.', '-']).filter_map(|p| p.parse().ok()).collect();
    nums.resize(4, 0);
    let commas = nums.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let icon = manifest.join("assets/icon.ico").to_string_lossy().replace('\\', "/");
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "imessage-linux"
      VALUE "FileDescription", "imsg portable client"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "imsg-app"
      VALUE "OriginalFilename", "imsg-app.exe"
      VALUE "ProductName", "imsg"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out.join("imsg-app.rc");
    std::fs::write(&rc_path, rc).unwrap();
    let obj = out.join("imsg-app-res.o");
    let target = std::env::var("TARGET").unwrap();
    let windres = std::env::var(format!("WINDRES_{}", target.replace('-', "_")))
        .or_else(|_| std::env::var("WINDRES"))
        .unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let status = std::process::Command::new(&windres).arg("-O").arg("coff").arg("-i").arg(&rc_path).arg("-o").arg(&obj).status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        // No icon is not worth failing the build for.
        other => println!("cargo:warning=windres ({windres}) failed: {other:?}; building without icon/version resource"),
    }
}
