//! Copies memory.x into a directory where the linker can always find it

use std::{env, fs, path::PathBuf};

fn main() {
	let out_directory = PathBuf::from(env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));
	fs::write(out_directory.join("memory.x"), include_bytes!("memory.x"))
		.expect("the build cannot continue without the linker script in place");
	println!("cargo:rustc-link-search={}", out_directory.display());
	println!("cargo:rerun-if-changed=memory.x");
	println!("cargo:rerun-if-changed=build.rs");
}
