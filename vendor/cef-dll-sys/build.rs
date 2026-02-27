#[cfg(not(feature = "dox"))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use download_cef::{CefFile, CefIndex, OsAndArch};
    use sha1_smol::Sha1;
    use std::{
        env,
        fs,
        io::{BufReader, Read},
        path::{Path, PathBuf},
    };

    fn calculate_file_sha1(path: &Path) -> anyhow::Result<String> {
        let mut file = BufReader::new(fs::File::open(path)?);
        let mut sha1 = Sha1::new();
        let mut buffer = [0; 8192];

        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            sha1.update(&buffer[..count]);
        }

        Ok(sha1.digest().to_string())
    }

    fn archive_name_from_url(url: &str) -> anyhow::Result<String> {
        let base = url.split('?').next().unwrap_or(url);
        let name = base
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .context("archive URL must include a file name")?;
        Ok(name.to_string())
    }

    fn download_archive_file(
        archive_url: &str,
        out_dir: &Path,
        archive_name: &str,
        expected_sha1: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        let archive_path = out_dir.join(archive_name);
        if archive_path.exists() {
            if let Some(expected_sha1) = expected_sha1 {
                let current_sha1 = calculate_file_sha1(&archive_path)?;
                if current_sha1 == expected_sha1 {
                    return Ok(archive_path);
                }
                fs::remove_file(&archive_path)?;
            } else {
                println!(
                    "cargo::warning=Using existing CEF archive without checksum verification: {}",
                    archive_path.display()
                );
                return Ok(archive_path);
            }
        }

        let response = ureq::get(archive_url).call()?;
        let mut reader = response.into_body().into_reader();
        let mut output = fs::File::create(&archive_path)?;
        std::io::copy(&mut reader, &mut output)?;

        if let Some(expected_sha1) = expected_sha1 {
            let downloaded_sha1 = calculate_file_sha1(&archive_path)?;
            if downloaded_sha1 != expected_sha1 {
                return Err(anyhow::anyhow!(
                    "SHA1 mismatch for {archive_name}: expected {expected_sha1}, got {downloaded_sha1}",
                ));
            }
        } else {
            println!(
                "cargo::warning=Downloaded CEF archive without checksum verification: {archive_name}",
            );
        }

        Ok(archive_path)
    }

    println!("cargo::rerun-if-changed=build.rs");

    let target = env::var("TARGET")?;
    let os_arch = OsAndArch::try_from(target.as_str())?;

    println!("cargo::rerun-if-env-changed=FLATPAK");
    println!("cargo::rerun-if-env-changed=CEF_PATH");
    println!("cargo::rerun-if-env-changed=CEF_DOWNLOAD_URL");
    println!("cargo::rerun-if-env-changed=CEF_DISTRIBUTION");
    println!("cargo::rerun-if-env-changed=CEF_ARCHIVE_NAME");
    println!("cargo::rerun-if-env-changed=CEF_ARCHIVE_URL");
    println!("cargo::rerun-if-env-changed=CEF_ARCHIVE_SHA1");
    println!("cargo::rerun-if-env-changed=CEF_SKIP_ARCHIVE_CHECK");
    let cef_path_env = env::var("FLATPAK")
        .map(|_| String::from("/usr/lib"))
        .or_else(|_| env::var("CEF_PATH"));

    let skip_archive_check = env::var("CEF_SKIP_ARCHIVE_CHECK")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    let cef_dir = match cef_path_env {
        Ok(cef_path) => {
            // Allow overriding the CEF path with environment variables.
            println!("Using CEF path from environment: {cef_path}");
            let archive_json = PathBuf::from(&cef_path).join("archive.json");
            if skip_archive_check {
                println!("Skipping CEF archive.json validation (CEF_SKIP_ARCHIVE_CHECK=1)");
            } else if archive_json.exists() {
                download_cef::check_archive_json(&env::var("CARGO_PKG_VERSION")?, &cef_path)?;
            } else {
                println!(
                    "cargo::warning=archive.json not found in CEF_PATH, skipping version check: {}",
                    archive_json.display()
                );
            }
            PathBuf::from(cef_path)
        }
        Err(_) => {
            let out_dir = PathBuf::from(env::var("OUT_DIR")?);
            let cef_dir = os_arch.to_string();
            let cef_dir = out_dir.join(&cef_dir);
            let download_url =
                env::var("CEF_DOWNLOAD_URL").unwrap_or_else(|_| download_cef::default_download_url());
            let requested_distribution =
                env::var("CEF_DISTRIBUTION").unwrap_or_else(|_| "standard".to_string());
            let requested_archive = env::var("CEF_ARCHIVE_NAME").ok();
            let requested_archive_url = env::var("CEF_ARCHIVE_URL").ok();
            let requested_archive_sha1 = env::var("CEF_ARCHIVE_SHA1").ok();

            if !fs::exists(&cef_dir)? {
                if let Some(archive_url) = requested_archive_url.as_deref() {
                    let archive_name = requested_archive
                        .clone()
                        .map(Ok)
                        .unwrap_or_else(|| archive_name_from_url(archive_url))?;
                    let archive = download_archive_file(
                        archive_url,
                        &out_dir,
                        &archive_name,
                        requested_archive_sha1.as_deref(),
                    )?;
                    let extracted_dir =
                        download_cef::extract_target_archive(&target, &archive, &out_dir, false)?;
                    if extracted_dir != cef_dir {
                        return Err(anyhow::anyhow!(
                            "extracted dir {extracted_dir:?} does not match cef_dir {cef_dir:?}",
                        ));
                    }

                    let archive_sha1 = match requested_archive_sha1 {
                        Some(sha1) => sha1,
                        None => calculate_file_sha1(&archive)?,
                    };
                    CefFile {
                        file_type: requested_distribution.clone(),
                        name: archive_name,
                        sha1: archive_sha1,
                    }
                    .write_archive_json(extracted_dir)?;
                } else {
                    let cef_version = download_cef::default_version(&env::var("CARGO_PKG_VERSION")?);
                    let index = CefIndex::download_from(&download_url)?;
                    let platform = index.platform(&target)?;
                    let version = platform.version(&cef_version)?;
                    let file = if let Some(requested_archive) = &requested_archive {
                        version
                            .files
                            .iter()
                            .find(|f| &f.name == requested_archive)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Requested CEF archive not found: {requested_archive}"
                                )
                            })?
                    } else {
                        version
                            .files
                            .iter()
                            .find(|f| f.file_type == requested_distribution)
                            .unwrap_or_else(|| {
                                eprintln!(
                                    "cargo::warning=CEF distribution '{}' not found, falling back to minimal",
                                    requested_distribution
                                );
                                version
                                    .minimal()
                                    .expect("minimal CEF archive must exist in index")
                            })
                    };

                    let archive_url = format!("{download_url}/{}", file.name);
                    let archive =
                        download_archive_file(&archive_url, &out_dir, &file.name, Some(&file.sha1))?;
                    let extracted_dir =
                        download_cef::extract_target_archive(&target, &archive, &out_dir, false)?;
                    if extracted_dir != cef_dir {
                        return Err(anyhow::anyhow!(
                            "extracted dir {extracted_dir:?} does not match cef_dir {cef_dir:?}",
                        ));
                    }

                    file.write_archive_json(extracted_dir)?;
                }
            }

            cef_dir
        }
    };

    let cef_dir = cef_dir.display().to_string();

    println!("cargo::metadata=CEF_DIR={cef_dir}");
    println!("cargo::rustc-link-search=native={cef_dir}");

    let mut cef_dll_wrapper = cmake::Config::new(&cef_dir);
    cef_dll_wrapper
        .generator("Ninja")
        .profile("RelWithDebInfo")
        .build_target("libcef_dll_wrapper");

    let project_arch = match os_arch.arch {
        "aarch64" => "arm64",
        arch => arch,
    };

    let sandbox = if cfg!(feature = "sandbox") {
        "ON"
    } else {
        "OFF"
    };

    match os_arch.os {
        "linux" => {
            println!("cargo::rustc-link-lib=dylib=cef");
        }
        "windows" => {
            let sdk_libs = [
                "comctl32.lib",
                "delayimp.lib",
                "mincore.lib",
                "powrprof.lib",
                "propsys.lib",
                "runtimeobject.lib",
                "setupapi.lib",
                "shcore.lib",
                "shell32.lib",
                "shlwapi.lib",
                "user32.lib",
                "version.lib",
                "wbemuuid.lib",
                "winmm.lib",
            ]
            .join(" ");

            let build_dir = cef_dll_wrapper
                .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded")
                .define("CMAKE_OBJECT_PATH_MAX", "500")
                .define("CMAKE_STATIC_LINKER_FLAGS", &sdk_libs)
                .define("PROJECT_ARCH", project_arch)
                .define("USE_SANDBOX", sandbox)
                .build()
                .display()
                .to_string();

            println!("cargo::rustc-link-search=native={build_dir}/build/libcef_dll_wrapper");
            println!("cargo::rustc-link-lib=static=libcef_dll_wrapper");

            println!("cargo::rustc-link-lib=dylib=libcef");
        }
        "macos" => {
            println!("cargo::rustc-link-lib=framework=AppKit");

            let build_dir = cef_dll_wrapper
                .no_default_flags(true)
                .define("PROJECT_ARCH", project_arch)
                .define("USE_SANDBOX", sandbox)
                .build()
                .display()
                .to_string();
            println!("cargo::rustc-link-search=native={build_dir}/build/libcef_dll_wrapper");
            println!("cargo::rustc-link-lib=static=cef_dll_wrapper");
        }
        os => unimplemented!("unknown target {os}"),
    }

    Ok(())
}

#[cfg(feature = "dox")]
fn main() {}
