// Builds the dashboard (dashboard/, Preact + Vite): dashboard/dist is embedded
// in the binary. Only needed with the `dashboard` feature; to skip it (use a prebuilt dist), set
// SKIP_DASHBOARD_BUILD=1.
use std::path::Path;
use std::process::Command;

fn main() {
    if std::env::var_os("CARGO_FEATURE_DASHBOARD").is_none() {
        return;
    }
    let dashboard = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard");
    for f in ["src", "index.html", "package.json", "package-lock.json", "vite.config.ts", "tsconfig.json"] {
        println!("cargo:rerun-if-changed={}", dashboard.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=SKIP_DASHBOARD_BUILD");

    if std::env::var_os("SKIP_DASHBOARD_BUILD").is_some() {
        return;
    }
    if !dashboard.join("node_modules").exists() {
        run(&dashboard, &["ci"]);
    }
    run(&dashboard, &["run", "build"]);
}

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("npm").args(args).current_dir(dir).status().unwrap_or_else(|e| {
        panic!(
            "failed to run `npm {}`: {e}. \
             Set SKIP_DASHBOARD_BUILD=1 to use an already built dashboard/dist",
            args.join(" ")
        )
    });
    if !status.success() {
        panic!("`npm {}` failed in {}", args.join(" "), dir.display());
    }
}
