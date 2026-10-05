use std::{
  ffi::OsString,
  io,
  path::{Path, PathBuf},
};

const FRAMEWORK: &str = "Chromium Embedded Framework.framework";

pub(crate) struct MacCefPaths {
  pub framework: PathBuf,
  pub main_bundle: Option<PathBuf>,
  pub subprocess: PathBuf,
}

impl MacCefPaths {
  pub fn resolve(executable: &Path, cef_path: Option<&Path>) -> io::Result<Self> {
    let directory = executable
      .parent()
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "CEF executable has no parent"))?;
    let contents = directory.parent().filter(|contents| {
      directory.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && contents
          .parent()
          .is_some_and(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
    });
    let (main_bundle, subprocess) = if let Some(contents) = contents {
      let name = executable
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "CEF executable has no name"))?
        .to_str()
        .ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "CEF executable name is not UTF-8",
          )
        })?;
      let helper_name = format!("{name} Helper");
      let helper = contents
        .join("Frameworks")
        .join(format!("{helper_name}.app/Contents/MacOS/{helper_name}"));
      if !helper.is_file() {
        return Err(io::Error::new(
          io::ErrorKind::NotFound,
          format!("Packaged CEF helper is missing: {}", helper.display()),
        ));
      }
      (Some(contents.parent().unwrap().to_path_buf()), helper)
    } else {
      (None, executable.to_path_buf())
    };
    let framework = resolve_framework(&directory.join("../Frameworks"), cef_path)?;
    Ok(Self {
      framework,
      main_bundle,
      subprocess,
    })
  }
}

pub(crate) fn helper_framework(executable: &Path, cef_path: Option<&Path>) -> io::Result<PathBuf> {
  let directory = executable
    .parent()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "CEF helper has no parent"))?;
  // Helpers live in Frameworks/<binary> Helper.app/Contents/MacOS.
  let bundle = directory.parent().and_then(Path::parent);
  let nested_helper = bundle
    .and_then(Path::parent)
    .and_then(Path::file_name)
    .is_some_and(|name| name == "Frameworks");
  let framework_directory = if nested_helper {
    directory.join("../../..")
  } else {
    // A main executable may also be used as the subprocess entry point.
    directory.join("../Frameworks")
  };
  resolve_framework(&framework_directory, cef_path)
}

pub(crate) fn helper_sandbox_enabled(arguments: impl IntoIterator<Item = OsString>) -> bool {
  !arguments.into_iter().any(|argument| {
    argument
      .to_str()
      .is_some_and(|argument| argument == "--no-sandbox" || argument.starts_with("--no-sandbox="))
  })
}

fn resolve_framework(directory: &Path, cef_path: Option<&Path>) -> io::Result<PathBuf> {
  let framework = match cef_path {
    Some(path) if path.file_name().is_some_and(|name| name == FRAMEWORK) => path.to_path_buf(),
    Some(path) => path.join(FRAMEWORK),
    None => directory.join(FRAMEWORK),
  }
  .canonicalize()?;
  if !framework.join("Chromium Embedded Framework").is_file() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!(
        "CEF framework executable is missing: {}",
        framework.display()
      ),
    ));
  }
  Ok(framework)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
  };

  struct Fixture(PathBuf);
  impl Fixture {
    fn new() -> Self {
      let root = std::env::temp_dir().join(format!(
        "cef-macos-paths-{}-{}",
        std::process::id(),
        SystemTime::now()
          .duration_since(UNIX_EPOCH)
          .unwrap()
          .as_nanos()
      ));
      fs::create_dir_all(&root).unwrap();
      Self(root.canonicalize().unwrap())
    }
    fn file(&self, path: &str) -> PathBuf {
      let path = self.0.join(path);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, b"path fixture").unwrap();
      path
    }
  }
  impl Drop for Fixture {
    fn drop(&mut self) {
      fs::remove_dir_all(&self.0).unwrap();
    }
  }

  #[test]
  fn bundle_selects_helper_from_binary_name_and_actual_main_bundle() {
    let f = Fixture::new();
    let executable = f.file("Agent Computer.app/Contents/MacOS/computer-desktop");
    let helper = f.file("Agent Computer.app/Contents/Frameworks/computer-desktop Helper.app/Contents/MacOS/computer-desktop Helper");
    f.file("Agent Computer.app/Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework");
    let paths = MacCefPaths::resolve(&executable, None).unwrap();
    assert_eq!(paths.subprocess, helper);
    assert_eq!(paths.main_bundle.unwrap(), f.0.join("Agent Computer.app"));
    assert_eq!(
      paths.framework,
      f.0
        .join("Agent Computer.app/Contents/Frameworks")
        .join(FRAMEWORK)
    );
  }

  #[test]
  fn incomplete_bundle_does_not_launch_main_executable_as_helper() {
    let f = Fixture::new();
    let executable = f.file("Agent Computer.app/Contents/MacOS/computer-desktop");
    assert_eq!(
      MacCefPaths::resolve(&executable, None)
        .err()
        .unwrap()
        .kind(),
      io::ErrorKind::NotFound
    );
  }

  #[test]
  fn development_executable_accepts_runtime_root_or_direct_framework() {
    let f = Fixture::new();
    let executable = f.file("target/release/computer-desktop");
    f.file("runtime/Chromium Embedded Framework.framework/Chromium Embedded Framework");
    let root = f.0.join("runtime");
    let paths = MacCefPaths::resolve(&executable, Some(&root)).unwrap();
    assert!(paths.main_bundle.is_none());
    assert_eq!(paths.subprocess, executable);
    assert_eq!(paths.framework, root.join(FRAMEWORK));
    assert_eq!(
      MacCefPaths::resolve(&executable, Some(&root.join(FRAMEWORK)))
        .unwrap()
        .framework,
      paths.framework
    );
  }

  #[test]
  fn explicit_incomplete_framework_is_rejected() {
    let f = Fixture::new();
    let executable = f.file("target/release/computer-desktop");
    fs::create_dir_all(f.0.join("runtime").join(FRAMEWORK)).unwrap();
    assert_eq!(
      MacCefPaths::resolve(&executable, Some(&f.0.join("runtime")))
        .err()
        .unwrap()
        .kind(),
      io::ErrorKind::NotFound
    );
  }

  #[test]
  fn helper_framework_preserves_bundle_path_and_development_override() {
    let f = Fixture::new();
    let helper = f.file("Agent Computer.app/Contents/Frameworks/computer-desktop Helper.app/Contents/MacOS/computer-desktop Helper");
    f.file("Agent Computer.app/Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework");
    assert_eq!(
      helper_framework(&helper, None).unwrap(),
      f.0
        .join("Agent Computer.app/Contents/Frameworks")
        .join(FRAMEWORK)
    );
    let main = f.file("Agent Computer.app/Contents/MacOS/computer-desktop");
    assert_eq!(
      helper_framework(&main, None).unwrap(),
      helper_framework(&helper, None).unwrap()
    );
    let executable = f.file("target/release/computer-desktop");
    f.file("runtime/Chromium Embedded Framework.framework/Chromium Embedded Framework");
    assert_eq!(
      helper_framework(&executable, Some(&f.0.join("runtime"))).unwrap(),
      f.0.join("runtime").join(FRAMEWORK)
    );
  }

  #[test]
  fn helper_sandbox_respects_chromium_no_sandbox_switch() {
    let arguments = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
    assert!(helper_sandbox_enabled(arguments(&["--type=renderer"])));
    assert!(!helper_sandbox_enabled(arguments(&[
      "--type=renderer",
      "--no-sandbox"
    ])));
    assert!(!helper_sandbox_enabled(arguments(&["--no-sandbox=1"])));
    assert!(helper_sandbox_enabled(arguments(&["--some-no-sandbox"])));
  }
}
