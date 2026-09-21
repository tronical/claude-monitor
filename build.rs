use std::collections::HashMap;
use std::path::Path;

/// Settings baked into the firmware. They come from the git-ignored
/// `secrets.env`, or from the environment, which wins.
const SETTINGS: [&str; 3] = ["WIFI_SSID", "WIFI_PASSWORD", "CLAUDE_OAUTH_TOKEN"];

fn main() {
    let from_file = read_env_file(Path::new("secrets.env"));
    println!("cargo:rerun-if-changed=secrets.env");

    for name in SETTINGS {
        println!("cargo:rerun-if-env-changed={name}");
        let value = std::env::var(name).ok().or_else(|| from_file.get(name).cloned());
        if value.as_deref().unwrap_or_default().is_empty() && name != "WIFI_PASSWORD" {
            println!(
                "cargo:warning={name} is not set: copy secrets.env.example to secrets.env. \
                 The firmware will build but only show a configuration hint."
            );
        }
        println!("cargo:rustc-env={name}={}", value.unwrap_or_default());
    }

    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
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
