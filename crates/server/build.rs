use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("failed to locate workspace root");
    let wasm_manifest = manifest_dir.join("../wasm/Cargo.toml");
    let protocol_dir = manifest_dir.join("../protocol");
    let wasm_src_dir = manifest_dir.join("../wasm/src");
    let static_gen_dir = manifest_dir.join("static/gen");
    let generated_js = static_gen_dir.join("rsdirstat_wasm.js");
    let generated_wasm = static_gen_dir.join("rsdirstat_wasm_bg.wasm");

    println!("cargo:rerun-if-changed={}", wasm_manifest.display());
    println!("cargo:rerun-if-changed={}", wasm_src_dir.display());
    println!("cargo:rerun-if-changed={}", protocol_dir.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", protocol_dir.join("src").display());
    // Track the generated outputs so deleting them (e.g. a manual clean) forces a rebuild.
    println!("cargo:rerun-if-changed={}", generated_js.display());
    println!("cargo:rerun-if-changed={}", generated_wasm.display());
    println!("cargo:rerun-if-env-changed=WASM_BINDGEN");
    println!("cargo:rerun-if-env-changed=RSDIRSTAT_BUILD_WASM");

    fs::create_dir_all(&static_gen_dir).expect("failed to create static/gen directory");

    let force = env::var_os("RSDIRSTAT_BUILD_WASM").is_some();
    let have_artifacts = generated_js.is_file() && generated_wasm.is_file();
    let bindgen = env::var_os("WASM_BINDGEN").unwrap_or_else(|| OsString::from("wasm-bindgen"));

    // Auto-build the bundle whenever the wasm toolchain is available (this build script only
    // re-runs when the wasm/protocol sources change, so a rebuild here is always warranted).
    // When the toolchain is missing, fall back to a prebuilt bundle if one exists, otherwise
    // fail with install instructions.
    if !toolchain_available(&bindgen) {
        if have_artifacts && !force {
            println!(
                "cargo:warning=wasm toolchain not found; using prebuilt bundle in {}",
                static_gen_dir.display()
            );
            return;
        }
        panic!(
            "cannot build the wasm frontend: the wasm toolchain is unavailable and no prebuilt \
bundle exists in {}. Install it with:\n  rustup target add wasm32-unknown-unknown\n  cargo install wasm-bindgen-cli",
            static_gen_dir.display()
        );
    }

    let profile = env::var("PROFILE").expect("missing PROFILE");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let wasm_target_dir = workspace_dir.join("target/rsdirstat-wasm-build");

    let mut cargo_build = Command::new(cargo);
    cargo_build
        .arg("build")
        .arg("--manifest-path")
        .arg(&wasm_manifest)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .env("CARGO_TARGET_DIR", &wasm_target_dir)
        // Always produce a real wasm build, even when the outer command is `cargo clippy`
        // (which would otherwise have the nested build inherit clippy-driver and lint flags).
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER");
    match profile.as_str() {
        "debug" => {}
        "release" => {
            cargo_build.arg("--release");
        }
        other => {
            cargo_build.arg("--profile").arg(other);
        }
    }

    let build_status = cargo_build.status().expect("failed to run cargo build for wasm crate");
    assert!(
        build_status.success(),
        "wasm cargo build failed with status {build_status}"
    );

    let wasm_path = wasm_target_dir
        .join("wasm32-unknown-unknown")
        .join(&profile)
        .join("rsdirstat_wasm.wasm");
    assert!(wasm_path.is_file(), "expected wasm artifact at {}", wasm_path.display());

    let bindgen_status = Command::new(&bindgen)
        .arg("--target")
        .arg("web")
        .arg("--no-typescript")
        .arg("--out-dir")
        .arg(&static_gen_dir)
        .arg("--out-name")
        .arg("rsdirstat_wasm")
        .arg(&wasm_path)
        .status()
        .expect("failed to run wasm-bindgen");
    assert!(
        bindgen_status.success(),
        "wasm-bindgen failed with status {bindgen_status}"
    );
}

/// Best-effort check that both wasm-bindgen and the wasm32 target are installed.
fn toolchain_available(bindgen: &OsStr) -> bool {
    let bindgen_ok = Command::new(bindgen)
        .arg("--version")
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    bindgen_ok && wasm_target_installed()
}

fn wasm_target_installed() -> bool {
    // If rustup manages the toolchain, trust its installed-target list. Otherwise assume the
    // target is available and let the cargo build surface a clear error if it is not.
    match Command::new("rustup").args(["target", "list", "--installed"]).output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == "wasm32-unknown-unknown"),
        _ => true,
    }
}
