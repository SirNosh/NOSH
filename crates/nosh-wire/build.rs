//! Embeds the vendored TypeScript JSON Schemas (conformance/schemas/json) into the crate.
use std::{env, fs, path::Path};

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/schemas/json");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("conformance/schemas/json is missing; run node scripts/sync-schemas.mjs")
        .map(|entry| entry.expect("readable schema directory").path())
        .filter(|path| path.to_string_lossy().ends_with(".v1.schema.json"))
        .collect();
    entries.sort();
    let mut out = String::from("pub(crate) static SCHEMAS: &[(&str, &str)] = &[\n");
    for path in entries {
        let name = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".v1.schema.json", "");
        let path = path.canonicalize().expect("schema path");
        out.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("schemas.rs"),
        out,
    )
    .expect("write schemas.rs");
}
