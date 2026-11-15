//! The installer's resources (the icon, the manifest: current common controls, per-monitor
//! DPI) and what it carries. (The manifest does not ask for elevation: the test program gets
//! the same manifest and could not be started then. main() asks instead.)
//!
//! HYPERWIN_PAYLOAD names a folder with what to install (hyperwin.exe, hyperwin-app.exe,
//! hyperwin-guest.exe, and whatever else lies there: the licenses and notices, krdpclient.exe
//! for the console): its files are put into the installer itself (tools\build-release.ps1 sets it). Without the variable (an
//! ordinary build of the workspace) the installer carries nothing and takes the programs that
//! lie beside it.

use std::io::Write as _;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="hyperWIN.Setup" version="1.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// The programs the installer must carry.
const PROGRAMS: [&str; 3] = ["hyperwin.exe", "hyperwin-app.exe", "hyperwin-guest.exe"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/hyperwin.ico");
    println!("cargo:rerun-if-env-changed=HYPERWIN_PAYLOAD");
    let out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("payload.rs");
    let mut source = std::fs::File::create(out).expect("payload.rs");
    writeln!(source, "pub const PAYLOAD: &[(&str, &[u8])] = &[").unwrap();
    if let Ok(folder) = std::env::var("HYPERWIN_PAYLOAD") {
        let folder = std::path::PathBuf::from(folder);
        println!("cargo:rerun-if-changed={}", folder.display());
        for name in PROGRAMS {
            assert!(
                folder.join(name).exists(),
                "HYPERWIN_PAYLOAD: {name} is not in {}",
                folder.display()
            );
        }
        // everything in the folder: the programs, and the licenses and notices that go with them
        let mut files: Vec<_> = std::fs::read_dir(&folder)
            .expect("HYPERWIN_PAYLOAD")
            .map(|e| e.expect("HYPERWIN_PAYLOAD").path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            println!("cargo:rerun-if-changed={}", file.display());
            writeln!(
                source,
                "    ({name:?}, include_bytes!({:?})),",
                file.display().to_string()
            )
            .unwrap();
        }
    }
    writeln!(source, "];").unwrap();
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon_with_id("../../assets/hyperwin.ico", "1")
            .set_manifest(MANIFEST)
            .compile()
            .expect("compile the installer's resources (rc.exe from the Windows SDK)");
    }
}
