use std::collections::HashMap;
use std::path::Path;

/// Optional settings baked into the firmware, from the git-ignored
/// `secrets.env` or from the environment, which wins. Without them the device
/// starts in setup mode and is configured from a phone instead.
const SETTINGS: [&str; 3] = ["WIFI_SSID", "WIFI_PASSWORD", "CLAUDE_OAUTH_TOKEN"];

/// The `--features` that pick the board, and the scale factor each gives the
/// 320x240 UI. The T4-S3's 600x450 panel is exactly 1.875 times that, so the
/// same layout fills it; glyphs and images are pre-rendered at that scale.
const BOARDS: [(&str, f32); 2] = [("esp32-s3-box-3", 1.0), ("lilygo-t4-s3", 1.875)];

fn main() {
    let chosen: Vec<_> = BOARDS
        .iter()
        .filter(|(name, _)| {
            let var = format!("CARGO_FEATURE_{}", name.to_uppercase().replace('-', "_"));
            std::env::var_os(var).is_some()
        })
        .collect();
    let &[&(_, scale_factor)] = chosen.as_slice() else {
        let names: Vec<_> = BOARDS.iter().map(|(name, _)| *name).collect();
        panic!(
            "pick exactly one board, e.g. `cargo run --release --features {}`; the boards are: {}",
            names[0],
            names.join(", ")
        );
    };

    let from_file = read_env_file(Path::new("secrets.env"));
    println!("cargo:rerun-if-changed=secrets.env");

    for name in SETTINGS {
        println!("cargo:rerun-if-env-changed={name}");
        let value = std::env::var(name).ok().or_else(|| from_file.get(name).cloned());
        println!("cargo:rustc-env={name}={}", value.unwrap_or_default());
    }

    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
        .with_scale_factor(scale_factor);
    slint_build::compile_with_config("ui/main.slint", config).unwrap();
    slint_build::print_rustc_flags().unwrap();
}

/// `NAME=value` lines; `#` comments and blank lines are skipped. Surrounding
/// quotes are optional and removed.
fn read_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            (name.trim().to_owned(), value.to_owned())
        })
        .collect()
}
