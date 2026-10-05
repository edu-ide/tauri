#![cfg(target_os = "macos")]

use std::{collections::HashMap, fs, path::PathBuf, process::Command};
use tauri_bundler::{
  bundle_project, BundleBinary, BundleSettings, MacOsSettings, PackageSettings, PackageType,
  SettingsBuilder,
};

/// Packages existing real executables and a CEF distribution through the complete bundler.
#[test]
#[ignore = "requires CEF_BUNDLER_SMOKE_BINARY, CEF_BUNDLER_SMOKE_CLI, CEF_BUNDLER_SMOKE_SESSION, CEF_BUNDLER_SMOKE_OUTPUT and CEF_BUNDLER_SMOKE_CONFIG"]
fn cef_formal_computer_app() {
  let binary = PathBuf::from(std::env::var_os("CEF_BUNDLER_SMOKE_BINARY").unwrap());
  let cli = PathBuf::from(std::env::var_os("CEF_BUNDLER_SMOKE_CLI").unwrap());
  let session = PathBuf::from(std::env::var_os("CEF_BUNDLER_SMOKE_SESSION").unwrap());
  let output = PathBuf::from(std::env::var_os("CEF_BUNDLER_SMOKE_OUTPUT").unwrap());
  let config = PathBuf::from(std::env::var_os("CEF_BUNDLER_SMOKE_CONFIG").unwrap());
  let config_directory = config.parent().unwrap();
  let config: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
  let product_name = config["productName"].as_str().unwrap();
  let version = config["version"].as_str().unwrap();
  let identifier = config["identifier"].as_str().unwrap();
  let cef_path = PathBuf::from(std::env::var_os("CEF_PATH").unwrap());
  assert!(binary.is_file());
  assert!(cli.is_file());
  assert!(session.join("agent_tauri.py").is_file());
  fs::create_dir_all(&output).unwrap();
  fs::copy(&binary, output.join("computer-desktop")).unwrap();
  fs::copy(&cli, output.join("computer")).unwrap();
  let settings = SettingsBuilder::new()
    .project_out_directory(&output)
    .target("aarch64-apple-darwin".into())
    .package_types(vec![PackageType::MacOsBundle])
    .package_settings(PackageSettings {
      product_name: product_name.into(),
      version: version.into(),
      description: "Computer automation desktop".into(),
      homepage: None,
      authors: None,
      default_run: Some("computer-desktop".into()),
    })
    .bundle_settings(BundleSettings {
      identifier: Some(identifier.into()),
      icon: Some(
        config["bundle"]["icon"]
          .as_array()
          .unwrap()
          .iter()
          .map(|icon| {
            config_directory
              .join(icon.as_str().unwrap())
              .to_str()
              .unwrap()
              .into()
          })
          .collect(),
      ),
      macos: MacOsSettings {
        minimum_system_version: config["bundle"]["macOS"]["minimumSystemVersion"]
          .as_str()
          .map(str::to_owned),
        ..Default::default()
      },
      cef_path: Some(cef_path),
      resources_map: Some(HashMap::from([(
        session.to_str().unwrap().into(),
        "session/".into(),
      )])),
      ..Default::default()
    })
    // Distribution signing is intentionally a separate credential-dependent step.
    .no_sign(true)
    .binaries(vec![
      BundleBinary::new("computer-desktop".into(), true),
      BundleBinary::new("computer".into(), false),
    ])
    .build()
    .unwrap();
  let bundles = bundle_project(&settings).unwrap();
  assert_eq!(bundles.len(), 1);
  let app = &bundles[0].bundle_paths[0];
  let contents = app.join("Contents");
  assert_eq!(
    app.file_name().unwrap(),
    format!("{product_name}.app").as_str()
  );
  let info = plist::Value::from_file(contents.join("Info.plist")).unwrap();
  let info = info.as_dictionary().unwrap();
  assert_eq!(info["CFBundleIdentifier"].as_string().unwrap(), identifier);
  assert_eq!(
    info["CFBundleShortVersionString"].as_string().unwrap(),
    version
  );
  if let Some(minimum) = config["bundle"]["macOS"]["minimumSystemVersion"].as_str() {
    assert_eq!(info["LSMinimumSystemVersion"].as_string().unwrap(), minimum);
  }
  assert!(contents
    .join("Resources")
    .join(info["CFBundleIconFile"].as_string().unwrap())
    .is_file());
  assert_eq!(
    fs::read(contents.join("MacOS/computer")).unwrap(),
    fs::read(&cli).unwrap()
  );
  let framework = contents.join("Frameworks/Chromium Embedded Framework.framework");
  assert!(framework.join("Chromium Embedded Framework").is_file());
  assert!(contents.join("Resources/session/agent_tauri.py").is_file());
  for suffix in [
    " Helper",
    " Helper (GPU)",
    " Helper (Renderer)",
    " Helper (Plugin)",
    " Helper (Alerts)",
  ] {
    let name = format!("computer-desktop{suffix}");
    let helper = contents
      .join("Frameworks")
      .join(format!("{name}.app/Contents/MacOS/{name}"));
    let arch = Command::new("lipo")
      .arg("-archs")
      .arg(&helper)
      .output()
      .unwrap();
    assert!(
      arch.status.success(),
      "{}",
      String::from_utf8_lossy(&arch.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&arch.stdout).trim(), "arm64");
  }
  for library in ["libEGL.dylib", "libGLESv2.dylib", "libvk_swiftshader.dylib"] {
    let link = contents.join("MacOS").join(library);
    assert!(fs::read_link(&link).unwrap().is_relative());
    assert!(link
      .canonicalize()
      .unwrap()
      .starts_with(framework.canonicalize().unwrap()));
  }
  println!("CEF_FORMAL_APP={}", app.display());
}
