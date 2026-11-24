fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // llvm-rc (cargo xwin) resolves paths from the preprocessed copy in OUT_DIR, not the
        // project root, so the resource script names the icon by its absolute path.
        let icon =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?).join("assets/icon.ico");
        let icon = icon.to_string_lossy().replace('\\', "/");
        let rc = std::path::Path::new(&std::env::var("OUT_DIR")?).join("datacraft.rc");
        std::fs::write(&rc, format!("1 ICON \"{icon}\"\n"))?;
        embed_resource::compile(&rc, embed_resource::NONE).manifest_required()?;
    }
    Ok(())
}
