//! Builds upstream nchat + our C++ shim with CMake, the cxx bridge, and the Slint UI.
//!
//! Environment knobs (all optional):
//!   NCHAT_UI_WHATSAPP=0|1   build the WhatsApp backend (default: auto, needs Go >= 1.23)
//!   NCHAT_UI_TELEGRAM=0|1   build the Telegram backend (default: auto, needs gperf; slow first build)
//!   NCHAT_UI_SIGNAL=0|1     build the Signal backend (default: 0, AGPL)
//!   NCHAT_UI_TG_APIID=..    your own Telegram API id and hash (https://my.telegram.org),
//!   NCHAT_UI_TG_APIHASH=..  built into the binary; without them nchat's own key is used,
//!                           which is fine for personal builds but must not be used for
//!                           binaries you distribute (see README, "Telegram")
//!   NCHAT_UI_SHARED=1       link nchat as shared libraries next to the binary (target/lib)
//!                           instead of one self-contained binary (default)
//!   NCHAT_UI_JOBS=N         parallel C++ jobs (default: number of CPUs, max 8 because tdlib is RAM hungry)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    for path in ["shim", "src/ffi.rs", "ui", "nchat/.git/HEAD", "patches"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for var in ["NCHAT_UI_WHATSAPP", "NCHAT_UI_TELEGRAM", "NCHAT_UI_SIGNAL", "NCHAT_UI_JOBS", "NCHAT_UI_SHARED"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    for var in ["NCHAT_UI_TG_APIID", "NCHAT_UI_TG_APIHASH"] {
        println!("cargo:rerun-if-env-changed={var}");
        if let Ok(v) = env::var(var) {
            if !v.trim().is_empty() {
                println!("cargo:rustc-env={var}={}", v.trim());
            }
        }
    }

    if !manifest_dir.join("nchat/CMakeLists.txt").exists() {
        panic!("nchat submodule missing: run `git submodule update --init`");
    }

    apply_nchat_patches(&manifest_dir);

    // ---- Toolchain detection -------------------------------------------------------------
    let mut path_var = env::var("PATH").unwrap_or_default();
    let go_ok = match find_go() {
        Some((go_dir, version)) => {
            if !path_var.split(':').any(|p| Path::new(p) == go_dir) {
                path_var = format!("{}:{}", go_dir.display(), path_var);
            }
            version >= (1, 23)
        }
        None => false,
    };
    let gperf_ok = which("gperf", &path_var);

    let whatsapp = flag("NCHAT_UI_WHATSAPP").unwrap_or(go_ok);
    let telegram = flag("NCHAT_UI_TELEGRAM").unwrap_or(gperf_ok);
    let signal = flag("NCHAT_UI_SIGNAL").unwrap_or(false);
    let shared = flag("NCHAT_UI_SHARED").unwrap_or(false);
    if !whatsapp {
        println!("cargo:warning=WhatsApp backend disabled (needs Go >= 1.23, or set NCHAT_UI_WHATSAPP=1)");
    }
    if !telegram {
        println!("cargo:warning=Telegram backend disabled (needs gperf, or set NCHAT_UI_TELEGRAM=1)");
    }

    let jobs = env::var("NCHAT_UI_JOBS")
        .ok()
        .and_then(|j| j.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8));

    // ---- CMake: upstream nchat + nchatcore ------------------------------------------------
    let onoff = |b: bool| if b { "ON" } else { "OFF" };
    let mut cfg = cmake::Config::new(manifest_dir.join("shim"));
    cfg.profile("Release")
        .define("HAS_WHATSAPP", onoff(whatsapp))
        .define("HAS_TELEGRAM", onoff(telegram))
        .define("HAS_SIGNAL", onoff(signal))
        .define("HAS_DUMMY", "ON")
        .define("HAS_SHARED_LIBS", onoff(shared))
        .env("PATH", &path_var)
        .env("CMAKE_BUILD_PARALLEL_LEVEL", jobs.to_string())
        .build_target("nchatui_linkprobe");
    if !which("ninja", &path_var) {
        panic!("ninja is required (sudo apt install ninja-build)");
    }
    cfg.generator("Ninja");
    let dst = cfg.build();
    let cmake_build = dst.join("build");

    let paths = fs::read_to_string(cmake_build.join("out/paths.txt")).expect("paths.txt from CMake");
    let get = |key: &str| -> String {
        paths
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(String::from)
            .unwrap_or_else(|| panic!("{key} missing in paths.txt"))
    };
    let nchat_bin_dir = PathBuf::from(get("NCHAT_BIN_DIR"));
    let nchat_lib_dir = PathBuf::from(get("NCHAT_LIB_DIR"));

    // ---- cxx bridge ----------------------------------------------------------------------
    cxx_build::bridge("src/ffi.rs")
        .file("shim/bridge.cpp")
        .include("shim")
        .include("nchat/lib/common/src")
        .include("nchat/lib/ncutil/src")
        .std("c++17")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("nchatui_bridge");

    // Link order: bridge (above) -> nchatcore -> the rest of nchat
    if shared {
        println!("cargo:rustc-link-search=native={}", cmake_build.join("out").display());
        println!("cargo:rustc-link-lib=static=nchatcore");
        println!("cargo:rustc-link-search=native={}", nchat_lib_dir.display());
        println!("cargo:rustc-link-lib=dylib=ncutil");
        println!("cargo:rustc-link-lib=dylib=dl");
        println!("cargo:rustc-link-lib=dylib=pthread");
        // nchat dlopen()s backends from <exe dir>/../lib, and libncutil is found the same way.
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib");
        stage_shared(&out_dir, &nchat_lib_dir, &nchat_bin_dir);
    } else {
        // Everything static: reuse the exact link line CMake computed for the probe.
        println!("cargo:rustc-link-arg-bins=-Wl,--start-group");
        for arg in probe_link_args(&cmake_build) {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
        println!("cargo:rustc-link-arg-bins=-Wl,--end-group");
    }
    println!(
        "cargo:rustc-env=NCHAT_UI_FEATURES=whatsapp={} telegram={} signal={} static={}",
        get("WHATSAPP") == "ON",
        get("TELEGRAM") == "ON",
        get("SIGNAL") == "ON",
        !shared
    );

    // ---- Slint UI ------------------------------------------------------------------------
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .expect("slint compile");
}

/// Stages target/lib/*.so and target/bin/nchat for the shared-library layout.
fn stage_shared(out_dir: &Path, nchat_lib_dir: &Path, nchat_bin_dir: &Path) {
    // target/<profile>/nchat-ui  ->  target/lib/*.so  and  target/bin/nchat (stock TUI)
    let target_root = out_dir.ancestors().nth(4).expect("target dir").to_path_buf();
    let stage_lib = target_root.join("lib");
    let stage_bin = target_root.join("bin");
    fs::create_dir_all(&stage_lib).unwrap();
    fs::create_dir_all(&stage_bin).unwrap();
    for entry in fs::read_dir(nchat_lib_dir).unwrap().flatten() {
        let p = entry.path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name.contains(".so") {
            copy_preserving_links(&p, &stage_lib.join(&name));
        }
    }
    let stock_nchat = nchat_bin_dir.join("nchat");
    if stock_nchat.exists() {
        copy_preserving_links(&stock_nchat, &stage_bin.join("nchat"));
    }
}

/// Reads the link probe's LINK_LIBRARIES from build.ninja and makes relative paths absolute.
fn probe_link_args(cmake_build: &Path) -> Vec<String> {
    let ninja = fs::read_to_string(cmake_build.join("build.ninja")).expect("build.ninja");
    let mut in_probe = false;
    for line in ninja.lines() {
        if line.starts_with("build ") {
            in_probe = line.contains("nchatui_linkprobe:") || line.contains("nchatui_linkprobe ");
            in_probe &= line.contains("CXX_EXECUTABLE_LINKER");
            continue;
        }
        if in_probe {
            if let Some(libs) = line.trim_start().strip_prefix("LINK_LIBRARIES = ") {
                return libs
                    .split_whitespace()
                    .map(|tok| {
                        let p = Path::new(tok);
                        if !tok.starts_with('-') && p.is_relative() {
                            cmake_build.join(p).display().to_string()
                        } else {
                            tok.to_string()
                        }
                    })
                    .collect();
            }
        }
    }
    panic!("link line for nchatui_linkprobe not found in build.ninja");
}

/// Applies patches/nchat/*.patch to the nchat checkout (idempotent). A patch that neither
/// applies nor is already applied stops the build: upstream changed that code (or now
/// includes the fix), so the patch must be updated or removed.
fn apply_nchat_patches(manifest_dir: &Path) {
    let dir = manifest_dir.join("patches/nchat");
    let Ok(rd) = fs::read_dir(&dir) else { return };
    let mut patches: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().map_or(false, |e| e == "patch")).collect();
    patches.sort();
    let nchat = manifest_dir.join("nchat");
    let git_apply = |args: &[&str], patch: &Path| {
        Command::new("git")
            .arg("-C")
            .arg(&nchat)
            .arg("apply")
            .args(args)
            .arg(patch)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    for patch in patches {
        println!("cargo:rerun-if-changed={}", patch.display());
        // also rerun when a patched file changes (e.g. reverted by an nchat update)
        if let Ok(text) = fs::read_to_string(&patch) {
            for line in text.lines() {
                if let Some(path) = line.strip_prefix("+++ b/") {
                    println!("cargo:rerun-if-changed={}", nchat.join(path.trim()).display());
                }
            }
        }
        let name = patch.file_name().unwrap().to_string_lossy().to_string();
        if git_apply(&["--reverse", "--check"], &patch) {
            continue; // already applied
        }
        if git_apply(&["--check"], &patch) && git_apply(&[], &patch) {
            println!("cargo:warning=applied nchat patch {name}");
            continue;
        }
        panic!(
            "nchat patch {name} doesn't apply to this nchat version. Upstream changed the patched \
             code (or already includes the fix): update or remove patches/nchat/{name}"
        );
    }
}

fn flag(name: &str) -> Option<bool> {
    env::var(name).ok().map(|v| matches!(v.as_str(), "1" | "ON" | "on" | "true" | "yes"))
}

fn which(prog: &str, path_var: &str) -> bool {
    path_var.split(':').any(|d| Path::new(d).join(prog).is_file())
}

/// Finds a Go toolchain: PATH first, then Debian/Ubuntu versioned packages (golang-1.NN-go).
fn find_go() -> Option<(PathBuf, (u32, u32))> {
    let mut candidates: Vec<PathBuf> = env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(PathBuf::from)
        .collect();
    if let Ok(rd) = fs::read_dir("/usr/lib") {
        let mut versioned: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("go-")))
            .map(|p| p.join("bin"))
            .collect();
        versioned.sort();
        versioned.reverse();
        candidates.extend(versioned);
    }
    candidates.push(PathBuf::from("/usr/local/go/bin"));

    let mut best: Option<(PathBuf, (u32, u32))> = None;
    for dir in candidates {
        let go = dir.join("go");
        if !go.is_file() {
            continue;
        }
        let Ok(out) = Command::new(&go).arg("version").output() else { continue };
        let text = String::from_utf8_lossy(&out.stdout);
        // "go version go1.26.1 linux/amd64"
        let Some(ver) = text.split_whitespace().nth(2).and_then(|v| v.strip_prefix("go")) else { continue };
        let mut parts = ver.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let v = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if best.as_ref().map_or(true, |(_, bv)| v > *bv) {
            best = Some((dir, v));
        }
    }
    best
}

fn copy_preserving_links(src: &Path, dst: &Path) {
    let _ = fs::remove_file(dst);
    if let Ok(target) = fs::read_link(src) {
        std::os::unix::fs::symlink(target, dst).unwrap();
    } else {
        fs::copy(src, dst).unwrap();
    }
}
