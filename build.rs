//! Stamp the build version into the binary.
//!
//! The release packaging sets `DEMO_VERSION`; a plain `cargo build` falls back
//! to the dev default in `cli::VERSION`. We re-export the env var so the
//! compile picks it up via `option_env!`.

fn main() {
    if let Ok(v) = std::env::var("DEMO_VERSION") {
        println!("cargo:rustc-env=DEMO_VERSION={v}");
    }
    println!("cargo:rerun-if-env-changed=DEMO_VERSION");
}
