//! Embeds the icon in the exe.

fn main() -> Result<(), embed_resource::CompilationResult> {
    println!("cargo::rerun-if-changed=assets/viewfinder.ico");
    embed_resource::compile("assets/viewfinder.rc", embed_resource::NONE).manifest_required()
}
