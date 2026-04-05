use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/package-lock.json");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/static");
    println!("cargo:rerun-if-changed=web/svelte.config.js");
    println!("cargo:rerun-if-changed=web/vite.config.ts");

    let web_build = Path::new("web/build");
    if web_build.exists() {
        return;
    }

    let web_dir = Path::new("web");
    if !web_dir.join("package.json").exists() {
        return;
    }

    eprintln!("web/build not found — building frontend SPA...");

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    let status = Command::new(npm)
        .args(["ci"])
        .current_dir(web_dir)
        .status()
        .expect("failed to run `npm ci` — is Node.js installed?");
    assert!(status.success(), "`npm ci` failed");

    let status = Command::new(npm)
        .args(["run", "build"])
        .current_dir(web_dir)
        .status()
        .expect("failed to run `npm run build`");
    assert!(status.success(), "`npm run build` failed");
}
