use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("schemas"));
    std::fs::create_dir_all(&root)?;
    for (filename, schema) in sandhi_core::contract_schema_documents() {
        std::fs::write(root.join(filename), schema)?;
    }
    Ok(())
}
