// Copyright 2019-2025 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  env, fs,
  path::{Path, PathBuf},
  process::Command,
};

fn main() {
  let target = env::var("TARGET").unwrap_or_default();
  let host = env::var("HOST").unwrap_or_default();

  // Only build/embed the CEF helper when compiling `tauri-bundler` for macOS.
  if !target.contains("apple-darwin") {
    return;
  }

  // We need `lipo` and a functioning macOS toolchain to produce a universal Mach-O.
  if !host.contains("apple-darwin") {
    panic!(
      "Building tauri-bundler for macOS requires a macOS host to build/embed the CEF helper binary"
    );
  }

  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
  let bundler_manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

  let helper_root = bundler_manifest_dir
    .parent() // crates/
    .and_then(|p| p.parent()) // repo root
    .map(|p| p.join("cef-helper"))
    .expect("failed to compute cef-helper path");

  let helper_manifest = helper_root.join("Cargo.toml");
  let helper_main = helper_root.join("src").join("main.rs");
  let cef_sys_root = helper_root
    .parent()
    .unwrap()
    .join("vendor/cef-dll-sys")
    .canonicalize()
    .expect("vendored cef-dll-sys is missing");

  // Rebuild if the helper crate changes.
  println!("cargo:rerun-if-changed={}", helper_manifest.display());
  println!("cargo:rerun-if-changed={}", helper_main.display());
  println!("cargo:rerun-if-changed={}", cef_sys_root.display());
  println!("cargo:rerun-if-env-changed=CEF_PATH");
  println!("cargo:rerun-if-env-changed=CEF_BUILD_PATH");
  let cef_path = env::var_os("CEF_PATH").map(PathBuf::from);
  let cef_build_path = env::var_os("CEF_BUILD_PATH").map(PathBuf::from);
  let cef_build_root = cef_distribution_root(cef_path.as_deref(), cef_build_path.as_deref())
    .unwrap_or_else(|error| panic!("{error}"));

  // Copy the helper crate sources into OUT_DIR so any generated files (Cargo.lock, target dir)
  // stay out of the repo checkout.
  let helper_src_dir = out_dir.join("cef-helper-src");
  let helper_src_manifest = helper_src_dir.join("Cargo.toml");
  let helper_src_main = helper_src_dir.join("src").join("main.rs");
  fs::create_dir_all(helper_src_main.parent().unwrap())
    .expect("failed to create cef-helper-src directory");
  let manifest =
    fs::read_to_string(&helper_manifest).expect("failed to read cef-helper Cargo.toml");
  let manifest = helper_manifest_with_patch(&manifest, &cef_sys_root)
    .expect("failed to configure vendored cef-dll-sys for CEF helper");
  fs::write(&helper_src_manifest, manifest).expect("failed to write cef-helper Cargo.toml");
  fs::copy(&helper_main, &helper_src_main).expect("failed to copy cef-helper main.rs");

  let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

  let helper_target_dir = out_dir.join("cef-helper-target");
  let aarch64 = build_helper(
    &cargo,
    &helper_src_manifest,
    &helper_target_dir,
    "aarch64-apple-darwin",
    "tauri-cef-helper",
    cef_build_root.as_deref(),
  );
  let x86_64 = build_helper(
    &cargo,
    &helper_src_manifest,
    &helper_target_dir,
    "x86_64-apple-darwin",
    "tauri-cef-helper",
    cef_build_root.as_deref(),
  );

  // Generate a small rust shim that exposes the embedded helper bytes.
  let shim_path = out_dir.join("cef_helpers.rs");
  let shim = format!(
    "pub const CEF_HELPER_AARCH64: &[u8] = include_bytes!(r#\"{}\"#);\n\
pub const CEF_HELPER_X86_64: &[u8] = include_bytes!(r#\"{}\"#);\n",
    aarch64.display(),
    x86_64.display()
  );
  fs::write(&shim_path, shim).expect("failed to write cef_helpers.rs");
}

fn build_helper(
  cargo: &str,
  manifest_path: &Path,
  target_dir: &Path,
  target: &str,
  bin_name: &str,
  cef_build_root: Option<&Path>,
) -> PathBuf {
  let mut cmd = Command::new(cargo);
  cmd
    .arg("build")
    .arg("--release")
    .arg("--manifest-path")
    .arg(manifest_path)
    .arg("--bin")
    .arg(bin_name)
    .arg("--target")
    .arg(target)
    .env("CARGO_TARGET_DIR", target_dir)
    .current_dir(manifest_path.parent().unwrap());
  if let Some(cef_build_root) = cef_build_root {
    cmd.env("CEF_PATH", cef_build_root);
  }

  let status = cmd
    .status()
    .expect("failed to spawn cargo build for CEF helper");
  if !status.success() {
    panic!("failed to build CEF helper for target {target}");
  }

  target_dir.join(target).join("release").join(bin_name)
}

fn helper_manifest_with_patch(manifest: &str, cef_sys_root: &Path) -> Result<String, String> {
  let path = cef_sys_root
    .to_str()
    .ok_or("vendored cef-dll-sys path is not UTF-8")?;
  let mut manifest: toml::Table = toml::from_str(manifest).map_err(|error| error.to_string())?;
  let dependency = manifest
    .get_mut("patch")
    .and_then(|value| value.get_mut("crates-io"))
    .and_then(|value| value.get_mut("cef-dll-sys"))
    .and_then(toml::Value::as_table_mut)
    .ok_or("cef-helper manifest must patch cef-dll-sys")?;
  dependency.insert("path".into(), toml::Value::String(path.into()));
  toml::to_string(&manifest).map_err(|error| error.to_string())
}

fn cef_distribution_root(
  cef_path: Option<&Path>,
  cef_build_path: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
  let Some(path) = cef_build_path.or(cef_path) else {
    return Ok(None);
  };
  let path = path
    .canonicalize()
    .map_err(|error| format!("CEF build path {}: {error}", path.display()))?;
  let root = if cef_build_path.is_some() || path.join("CMakeLists.txt").is_file() {
    path.clone()
  } else if path.file_name().is_some_and(|name| name == "Release") {
    path.parent().unwrap().to_path_buf()
  } else if path
    .file_name()
    .is_some_and(|name| name == "Chromium Embedded Framework.framework")
    && path
      .parent()
      .and_then(Path::file_name)
      .is_some_and(|name| name == "Release")
  {
    path.parent().unwrap().parent().unwrap().to_path_buf()
  } else {
    return Err(format!(
      "CEF helper needs the full CEF distribution (CMakeLists.txt, include, libcef_dll); {} contains only staged runtime files. Set CEF_BUILD_PATH to the distribution root while keeping CEF_PATH for bundling",
      path.display()
    ));
  };
  if !root.join("CMakeLists.txt").is_file()
    || !root.join("include").is_dir()
    || !root.join("libcef_dll").is_dir()
  {
    return Err(format!(
      "CEF_BUILD_PATH {} must contain CMakeLists.txt, include and libcef_dll",
      root.display()
    ));
  }
  Ok(Some(root))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn helper_patch_survives_standalone_source_copy() {
    let source = include_str!("../../cef-helper/Cargo.toml");
    let path = Path::new("/tmp/vendor 한글/cef-dll-sys \"quoted\"");
    let copied = helper_manifest_with_patch(source, path).unwrap();
    let manifest: toml::Table = toml::from_str(&copied).unwrap();
    assert_eq!(
      manifest["patch"]["crates-io"]["cef-dll-sys"]["path"].as_str(),
      path.to_str()
    );
    assert!(manifest.contains_key("workspace"));
    assert_eq!(
      manifest["dependencies"]["cef-dll-sys"]["version"].as_str(),
      Some("=144.1.0+144.0.7")
    );
    assert!(helper_manifest_with_patch("[workspace]", path).is_err());
  }

  fn distribution() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("CMakeLists.txt"), "project(cef)").unwrap();
    fs::create_dir(root.path().join("include")).unwrap();
    fs::create_dir(root.path().join("libcef_dll")).unwrap();
    fs::create_dir_all(
      root
        .path()
        .join("Release/Chromium Embedded Framework.framework"),
    )
    .unwrap();
    root
  }

  #[test]
  fn resolves_distribution_release_and_framework_paths() {
    let root = distribution();
    let expected = Some(root.path().canonicalize().unwrap());
    for path in [
      root.path().to_path_buf(),
      root.path().join("Release"),
      root
        .path()
        .join("Release/Chromium Embedded Framework.framework"),
    ] {
      assert_eq!(cef_distribution_root(Some(&path), None).unwrap(), expected);
    }
    assert_eq!(cef_distribution_root(None, None).unwrap(), None);
  }

  #[test]
  fn staged_framework_requires_explicit_distribution() {
    let staged = tempfile::tempdir().unwrap();
    fs::create_dir(staged.path().join("Chromium Embedded Framework.framework")).unwrap();
    assert!(cef_distribution_root(Some(staged.path()), None)
      .unwrap_err()
      .contains("CEF_BUILD_PATH"));
    let root = distribution();
    assert_eq!(
      cef_distribution_root(Some(staged.path()), Some(root.path())).unwrap(),
      Some(root.path().canonicalize().unwrap())
    );
    assert!(cef_distribution_root(Some(root.path()), Some(staged.path())).is_err());
  }
}
