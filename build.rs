use std::env;
use std::fs;
use std::path::Path;

fn scan_dir_recursive(dir: &Path, dest_prefix: &str) -> Vec<(String, String, String)> {
    let mut entries = Vec::new();
    if !dir.exists() {
        return entries;
    }
    for entry in fs::read_dir(dir)
        .expect("Failed to read resource directory")
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read_to_string(&path).expect("Failed to read resource file");
            let dest = format!("{}{}", dest_prefix, name);
            entries.push((name, content, dest));
        } else if path.is_dir() {
            let sub_prefix = format!("{}{}/", dest_prefix, path.file_name().unwrap().to_string_lossy());
            entries.extend(scan_dir_recursive(&path, &sub_prefix));
        }
    }
    entries
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let templates_dir = Path::new(&manifest_dir).join("resources/templates");
    let skills_dir = Path::new(&manifest_dir).join("resources/skills");

    let mut all_entries = Vec::new();
    all_entries.extend(scan_dir_recursive(&templates_dir, ""));
    all_entries.extend(scan_dir_recursive(&skills_dir, "skills/"));

    let webui_dir = Path::new(&manifest_dir).join("resources/webui");
    all_entries.extend(scan_dir_recursive(&webui_dir, "webui/"));

    all_entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut code = String::new();
    code.push_str(
        "/// Auto-generated embedded resource: (filename, content, destination_path).\n",
    );
    code.push_str(
        "pub const EMBEDDED_FILES: &[(&str, &str, &str)] = &[\n"
    );
    for (name, content, dest) in &all_entries {
        code.push_str(&format!(
            "    ({:?}, {:?}, {:?}),\n",
            name, content, dest
        ));
    }
    code.push_str("];\n");

    code.push_str(
        r#"
/// Look up embedded content by filename.
pub fn get_content(name: &str) -> Option<&'static str> {
    EMBEDDED_FILES.iter().find(|(n, _, _)| *n == name).map(|(_, c, _)| *c)
}

/// Look up destination path by filename.
pub fn get_dest(name: &str) -> Option<&'static str> {
    EMBEDDED_FILES.iter().find(|(n, _, _)| *n == name).map(|(_, _, d)| *d)
}
"#,
    );

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("embed_gen.rs");
    fs::write(&dest, code).unwrap();

    println!("cargo:rerun-if-changed=resources/templates");
    println!("cargo:rerun-if-changed=resources/skills");
    println!("cargo:rerun-if-changed=resources/webui");
}
