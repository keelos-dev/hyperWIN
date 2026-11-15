//! hyperwin-guest.exe runs in Windows guests as they come from the installer, and those have
//! no vcruntime140.dll (it arrives with other software). So the Visual C++ runtime is linked
//! into the program; the universal C runtime stays a DLL, which every Windows 10 has.
//! (For this program alone: the rest of the workspace links C code built for the DLL runtime.)

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // (libucrt: what libcmt would bring along by itself)
        for left_out in ["msvcrt.lib", "vcruntime.lib", "libucrt.lib"] {
            println!("cargo:rustc-link-arg-bins=/NODEFAULTLIB:{left_out}");
        }
        for linked_in in ["libcmt.lib", "libvcruntime.lib", "ucrt.lib"] {
            println!("cargo:rustc-link-arg-bins=/DEFAULTLIB:{linked_in}");
        }
    }
}
