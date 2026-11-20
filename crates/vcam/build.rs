//! Keeps the COM exports out of the import library.

fn main() {
    // COM only finds DllGetClassObject and DllCanUnloadNow with GetProcAddress.
    println!("cargo::rustc-cdylib-link-arg=/IGNORE:4104");
}
