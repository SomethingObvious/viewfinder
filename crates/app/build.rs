//! Builds the icon into the exe, so Explorer, the taskbar and shortcuts show the mark.

fn main() -> Result<(), embed_resource::CompilationResult> {
    println!("cargo::rerun-if-changed=assets/viewfinder.ico");
    embed_resource::compile("assets/viewfinder.rc", embed_resource::NONE).manifest_required()
}
