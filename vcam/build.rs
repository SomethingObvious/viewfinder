//! Keeps the COM exports out of the import library.

fn main() {
    // The linker wants DllGetClassObject and DllCanUnloadNow kept out of the
    // import library. COM only ever finds them with GetProcAddress.
    println!("cargo::rustc-cdylib-link-arg=/IGNORE:4104");
}
