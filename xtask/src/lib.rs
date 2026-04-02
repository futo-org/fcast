// TODO: create a ytool that doesn't require huge uniffi

#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::rc::Rc;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use camino::{Utf8Path, Utf8PathBuf};
#[cfg(target_os = "macos")]
use clap::Args;
use xshell::Shell;

pub mod android;
#[cfg(feature = "uniffi")]
pub mod csharp;
#[cfg(feature = "uniffi")]
pub mod kotlin;
pub mod mdns;
pub mod receiver;
pub mod sender;
#[cfg(feature = "uniffi")]
pub mod swift;
#[allow(unused_imports)]
pub mod test_corpus;
pub mod workspace;

thread_local! {
    static SH: Rc<Shell> = Rc::new(Shell::new().unwrap())
}

pub fn sh() -> Rc<Shell> {
    SH.with(|sh| sh.clone())
}

#[derive(clap::ValueEnum, Clone)]
pub enum AndroidAbiTarget {
    X64,
    X86,
    Arm64,
    Arm32,
}

impl AndroidAbiTarget {
    pub fn translate(&self) -> &'static str {
        match self {
            Self::X64 => "x86_64-linux-android",
            Self::X86 => "i686-linux-android",
            Self::Arm64 => "aarch64-linux-android",
            Self::Arm32 => "armv7-linux-androideabi",
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn concat_paths(paths: &[impl AsRef<Utf8Path>]) -> Utf8PathBuf {
    let mut res = Utf8PathBuf::new();
    res.extend(paths);
    res
}

#[cfg(target_os = "macos")]
#[derive(Args)]
pub struct BuildMacosInstallerArgs {
    #[clap(long, default_value = "false")]
    pub sign: bool,
    #[clap(long)]
    pub p12_file: Option<String>,
    #[clap(long)]
    pub p12_password_file: Option<String>,
    #[clap(long)]
    pub api_key_file: Option<String>,
}

#[cfg(target_os = "macos")]
pub fn get_gstreamer_root_libs() -> Utf8PathBuf {
    let root = Utf8PathBuf::from("/Library/Frameworks/GStreamer.framework/Versions/1.0");
    assert!(root.exists(), "GStreamer is not installed");
    println!("GStreamer root is: {root:?}");
    root.join("lib")
}

#[cfg(target_os = "macos")]
pub fn is_macos_system_library(path: &str) -> bool {
    path.starts_with("/usr/lib/") || path.starts_with("/System/Library/") || path.contains(".asan.")
}

#[cfg(target_os = "macos")]
fn find_non_system_dependencies_with_otool(binary_path: &Utf8PathBuf) -> HashSet<Utf8PathBuf> {
    use std::{
        io::{BufRead, BufReader},
        process::Command,
    };

    let binary_path = binary_path.as_str();
    let mut cmd = Command::new("/usr/bin/otool");
    cmd.arg("-L").arg(&binary_path);
    let output = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();

    let stdout = output.stdout.expect("failed to capture otool stdout");
    let reader = BufReader::new(stdout);

    let mut result = HashSet::new();

    for line in reader.lines() {
        let line = line.unwrap();
        if !line.starts_with('\t') {
            continue;
        }
        if let Some((dep, _)) = line[1..].split_once(char::is_whitespace) {
            let dependency = dep.to_string();
            if !is_macos_system_library(&dependency) && !dependency.contains("librustc-stable_rt") {
                result.insert(Utf8PathBuf::from(dependency));
            }
        } else {
            let dependency = line[1..].to_string();
            if !is_macos_system_library(&dependency) && !dependency.contains("librustc-stable_rt") {
                result.insert(Utf8PathBuf::from(dependency));
            }
        }
    }

    result
}

#[cfg(target_os = "macos")]
pub fn rewrite_dependencies_to_be_relative(
    binary: &Utf8PathBuf,
    dependency_lines: &HashSet<Utf8PathBuf>,
    relative_path: &Utf8PathBuf,
) {
    for dep in dependency_lines {
        use std::process::Command;

        if is_macos_system_library(dep.as_str()) || dep.starts_with("@rpath/") {
            continue;
        }

        let basename = Utf8Path::new(dep)
            .file_name()
            .unwrap_or_else(|| dep.as_str());

        let new_path = Utf8PathBuf::from("@executable_path")
            .join(relative_path)
            .join(basename);

        let status = Command::new("install_name_tool")
            .arg("-change")
            .arg(dep)
            .arg(new_path.as_str())
            .arg(binary.as_str())
            .status()
            .unwrap();

        if !status.success() {
            panic!(
                "{:?} install_name_tool exited with return value {:?}",
                [
                    "install_name_tool",
                    "-change",
                    dep.as_str(),
                    new_path.as_str(),
                    binary.as_str()
                ],
                status.code(),
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub fn make_rpath_path_absolute(dylib_path_from_otool: &str, rpath: &Utf8Path) -> Utf8PathBuf {
    if !dylib_path_from_otool.starts_with("@rpath/") {
        return Utf8PathBuf::from(dylib_path_from_otool);
    }

    let path_relative_to_rpath = &dylib_path_from_otool["@rpath/".len()..];
    let candidates = ["", "..", "gstreamer-1.0"];

    for relative_directory in &candidates {
        let mut full = Utf8PathBuf::from(rpath);
        if !relative_directory.is_empty() {
            full.push(relative_directory);
        }
        full.push(path_relative_to_rpath);

        if full.exists() {
            let normalized = std::fs::canonicalize(&full)
                .map(|p| Utf8PathBuf::try_from(p).unwrap())
                .unwrap_or(full);
            return normalized;
        }
    }

    panic!(
        "Unable to satisfy rpath dependency: {}",
        dylib_path_from_otool
    );
}

#[cfg(target_os = "macos")]
pub fn find_libraries(binary_path: &Utf8PathBuf, plugins: Vec<String>) -> HashSet<Utf8PathBuf> {
    println!("############### Finding libraries ###############");

    let gstreamer_root_libs = crate::get_gstreamer_root_libs();
    let mut binary_dependencies = crate::find_non_system_dependencies_with_otool(&binary_path);
    for dep in plugins {
        let dep_path = gstreamer_root_libs.join("gstreamer-1.0").join(dep);
        assert!(dep_path.exists(), "Missing plugin `{dep_path}`");
        println!("Found `{dep_path}`");
        binary_dependencies.insert(dep_path);
    }
    binary_dependencies
}

#[cfg(target_os = "macos")]
pub fn process_dependencies(
    sh: &Rc<xshell::Shell>,
    binary_dependencies: HashSet<Utf8PathBuf>,
    library_target_directory: Utf8PathBuf,
) {
    use xshell::cmd;

    println!("############### Processing dependencies ###############");

    let gstreamer_root_libs = crate::get_gstreamer_root_libs();
    let relative_path = Utf8PathBuf::from("lib/");
    let mut pending_to_be_copied: HashSet<Utf8PathBuf> = binary_dependencies.clone();
    let mut already_copied: HashSet<Utf8PathBuf> = HashSet::new();
    while !pending_to_be_copied.is_empty() {
        let checking: HashSet<Utf8PathBuf> = pending_to_be_copied.drain().collect();

        for otool_dependency in checking {
            already_copied.insert(otool_dependency.clone());

            let original_dylib_path =
                crate::make_rpath_path_absolute(otool_dependency.as_str(), &gstreamer_root_libs);
            let transitive_dependencies = HashSet::from_iter(
                crate::find_non_system_dependencies_with_otool(&original_dylib_path).into_iter(),
            );

            let new_dylib_path = library_target_directory.join(
                Utf8Path::new(&original_dylib_path)
                    .file_name()
                    .unwrap_or(original_dylib_path.as_str()),
            );

            if !new_dylib_path.exists() {
                std::fs::copy(&original_dylib_path, &new_dylib_path).unwrap();
                crate::rewrite_dependencies_to_be_relative(
                    &new_dylib_path,
                    &transitive_dependencies,
                    &relative_path,
                );
                if !new_dylib_path.ends_with("libMoltenVK.dylib") {
                    cmd!(sh, "strip -x {new_dylib_path}").run().unwrap();
                }
            }

            let mut to_queue = transitive_dependencies;
            for seen in &already_copied {
                to_queue.remove(seen);
            }
            pending_to_be_copied.extend(to_queue);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn sign(
    sh: &Rc<xshell::Shell>,
    app_top_level: &Utf8PathBuf,
    executable_name: &str,
    p12_file: Option<String>,
    p12_password_file: Option<String>,
) {
    println!("############### Signing ###############");
    let p12_file = p12_file.unwrap();
    let p12_password_file = p12_password_file.unwrap();
    xshell::cmd!(
        sh,
        "rcodesign sign --p12-file {p12_file} --p12-password-file {p12_password_file} --code-signature-flags runtime {app_top_level}/Contents/MacOS/{executable_name}"
    ).run().unwrap();
    xshell::cmd!(
        sh,
        "rcodesign sign --p12-file {p12_file} --p12-password-file {p12_password_file} {app_top_level}"
    ).run().unwrap();
}

#[cfg(target_os = "macos")]
pub fn notarize(sh: &Rc<xshell::Shell>, api_key_file: Option<String>, path_to_dmg: &Utf8PathBuf) {
    println!("############### Notarizing ###############");
    let api_key_file = api_key_file.unwrap();
    xshell::cmd!(
        sh,
        "rcodesign notary-submit --api-key-file {api_key_file} --wait {path_to_dmg}"
    )
    .run()
    .unwrap();
}

#[cfg(target_os = "macos")]
pub enum AppType {
    Sender,
    Receiver,
}

#[cfg(target_os = "macos")]
pub fn create_package(
    sh: &Rc<xshell::Shell>,
    app_type: AppType,
    app_version: String,
    app_top_level: Utf8PathBuf,
    applications_link_path: Utf8PathBuf,
    path_to_dmg: Utf8PathBuf,
    path_to_dmg_dir: Utf8PathBuf,
    sign: bool,
    p12_file: Option<String>,
    p12_password_file: Option<String>,
    api_key_file: Option<String>,
) {
    use xshell::cmd;

    let (lower_case_name, app_name, upper_case_word_name) = match app_type {
        AppType::Sender => ("fcast-sender", "FCast Sender.app", "FCastSender"),
        AppType::Receiver => ("fcast-receiver", "FCast Receiver.app", "FCastReceiver"),
    };

    if sign {
        crate::sign(
            &sh,
            &app_top_level,
            lower_case_name,
            p12_file,
            p12_password_file,
        );
    }

    println!("############### Creating tarball ###############");

    cmd!(sh, "tar -czf target/{lower_case_name}-{app_version}-macos-aarch64.tar.gz -C {path_to_dmg_dir} {app_name}").run().unwrap();

    println!("############### Creating dmg ###############");

    std::os::unix::fs::symlink(Utf8PathBuf::from("/Applications"), applications_link_path).unwrap();

    cmd!(sh, "hdiutil create -volname {upper_case_word_name} -megabytes 250 {path_to_dmg} -srcfolder {path_to_dmg_dir}").run().unwrap();

    if sign {
        crate::notarize(&sh, api_key_file, &path_to_dmg);
    }
}

#[cfg(target_os = "windows")]
pub fn get_gst_root(sh: &Rc<xshell::Shell>) -> Utf8PathBuf {
    let root = Utf8PathBuf::from(
        sh.var("GSTREAMER_1_0_ROOT_MSVC_X86_64")
            .expect("GStreamer not found"),
    );
    assert!(root.exists(), "GStreamer installation is likely broken");
    println!("GStreamer root is: {root:?}");

    root
}

#[cfg(target_os = "windows")]
pub fn setup_build_dir(sh: &Rc<xshell::Shell>, root_path: &Utf8PathBuf) -> Utf8PathBuf {
    let build_dir_root = {
        let mut p = root_path.clone();
        p.extend(["target", "win-build"]);
        p
    };

    if sh.remove_path(&build_dir_root).is_ok() {
        println!("Removed old build dir at `{build_dir_root:?}`")
    }

    sh.create_dir(&build_dir_root).unwrap();

    build_dir_root
}

#[cfg(target_os = "windows")]
pub fn find_dlls(gst_root: &Utf8PathBuf, dlls: Vec<String>) -> Vec<(Utf8PathBuf, String)> {
    println!("############### Finding DLLs ###############");

    let mut res = Vec::new();
    for dll in dlls {
        let mut dll_path = gst_root.clone();
        dll_path.extend(["bin", &dll]);
        assert!(
            dll_path.exists(),
            "DLL `{dll}` is missing full-path={dll_path:?}"
        );
        println!("Required library found: `{dll}`");
        res.push((dll_path, dll));
    }

    res
}

#[cfg(target_os = "windows")]
pub fn find_plugins(gst_root: &Utf8PathBuf, plugins: Vec<String>) -> Vec<(Utf8PathBuf, String)> {
    println!("############### Finding plugins ###############");

    let mut res = Vec::new();
    for plugin in plugins {
        let mut plugin_path = gst_root.clone();
        plugin_path.extend(["lib", "gstreamer-1.0", &plugin]);
        assert!(
            plugin_path.exists(),
            "Plugin `{plugin}` is missing full-path={plugin_path:?}"
        );
        println!("Required plugin found: `{plugin}`");
        res.push((plugin_path, plugin));
    }

    res
}

#[cfg(target_os = "windows")]
pub fn find_vswhere(paths: &[Utf8PathBuf]) -> Option<Utf8PathBuf> {
    for path in paths {
        let vswhere = concat_paths(&[
            path.as_str(),
            "Microsoft Visual Studio",
            "Installer",
            "vswhere.exe",
        ]);
        if vswhere.exists() {
            return Some(vswhere);
        }
    }

    None
}

#[cfg(target_os = "windows")]
pub fn get_msvc_installations(sh: &Rc<Shell>) -> Vec<(String, Utf8PathBuf)> {
    let program_files = Utf8PathBuf::from(
        sh.var("POGRAMFILES")
            .unwrap_or("C:\\Program Files".to_string()),
    );
    let program_files_x86 = Utf8PathBuf::from(
        sh.var("ProgramFiles(x86)")
            .unwrap_or("C:\\Program Files (x86)".to_string()),
    );

    let vswhere =
        find_vswhere(&[program_files, program_files_x86]).expect("Could not find vswhere.exe");

    let output = Command::new(vswhere)
        .arg("-format")
        .arg("json")
        .arg("-products")
        .arg("*")
        .arg("-requires")
        .arg("Microsoft.VisualStudio.Component.VC.Tools.x86.x64")
        .arg("-requires")
        .arg("Microsoft.VisualStudio.Component.Windows*SDK.*")
        .arg("-utf8")
        .output()
        .expect("Failed to execute vswhere command");

    use std::process::Command;

    use serde_json::Value;

    let output_str = String::from_utf8_lossy(&output.stdout);
    let installations: Value = serde_json::from_str(&output_str).unwrap_or(Value::Null);

    let mut msvcs = Vec::new();
    if let Value::Array(installs) = installations {
        for install in installs {
            if let Value::Object(install_obj) = install {
                let installed_version = install_obj["installationVersion"]
                    .as_str()
                    .unwrap_or("")
                    .split('.')
                    .next()
                    .unwrap_or("");
                let installed_version = format!("{}.0", installed_version);

                if &installed_version != "17.0" && &installed_version != "18.0" {
                    continue;
                }

                let installation_path = install_obj["installationPath"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let vc_install_path = Utf8PathBuf::from(&installation_path).join("VC");

                msvcs.push((installation_path, vc_install_path));
            }
        }
    }

    msvcs
}

#[cfg(target_os = "windows")]
fn find_msvc_redist_dirs(sh: &Rc<Shell>) -> Vec<Utf8PathBuf> {
    let installations = get_msvc_installations(&sh);
    let mut results = Vec::new();
    for (_, installation) in installations {
        let redist_dir = installation.join("Redist").join("MSVC");
        if !redist_dir.is_dir() {
            continue;
        }

        let subdirectories = std::fs::read_dir(&redist_dir)
            .expect("Failed to read redist directory")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();

        for entry in subdirectories.iter().rev() {
            let redist_path = entry.path();
            for redist_version in ["VC141", "VC142", "VC143", "VC145", "VC150", "VC160"].iter() {
                let path1 = redist_path.join(format!("Microsoft.{}.CRT", redist_version));
                let path2 = redist_path
                    .join("onecore")
                    .join("x64")
                    .join(format!("Microsoft.{}.CRT", redist_version));

                for path in vec![path1, path2] {
                    if path.is_dir() {
                        results.push(Utf8PathBuf::try_from(path).unwrap());
                    }
                }
            }
        }
    }

    results
}

#[cfg(target_os = "windows")]
pub fn find_windows_sdk_installation_path() -> Utf8PathBuf {
    use winreg::{enums::*, RegKey};
    let key_path = r"SOFTWARE\Wow6432Node\Microsoft\Microsoft SDKs\Windows\v10.0";
    let hkml = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hkml.open_subkey(key_path).unwrap();
    let installation_folder: String = key.get_value("InstallationFolder").unwrap();
    Utf8PathBuf::from(installation_folder)
}

#[cfg(target_os = "windows")]
pub fn find_c_runtime(windows_sdk_dir: Utf8PathBuf) -> Vec<(Utf8PathBuf, String)> {
    use std::ffi::OsStr;
    let crt_dlls = ["api-ms-win-crt-runtime-l1-1-0.dll"];
    let mut paths = Vec::new();
    for dll_name in crt_dlls {
        for entry in walkdir::WalkDir::new(&windows_sdk_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() && entry.file_name() == OsStr::new(dll_name) {
                let path = entry.path();
                if path
                    .parent()
                    .map_or(false, |parent| parent.ends_with("x64"))
                {
                    paths.push((
                        Utf8PathBuf::from(Utf8Path::from_path(path).unwrap()),
                        dll_name.to_owned(),
                    ));
                    break;
                }
            }
        }
    }

    assert_eq!(
        paths.len(),
        crt_dlls.len(),
        "One or more CRT librarie(s) was not found (expected: {crt_dlls:?}, got: {paths:?})"
    );

    paths
}

#[cfg(target_os = "windows")]
pub fn find_msvc_redists(sh: &Rc<Shell>) -> Vec<(Utf8PathBuf, String)> {
    let mut res = Vec::new();
    let redist_dirs = crate::find_msvc_redist_dirs(&sh);
    let mut did_find_redists = false;
    for redist_dir in redist_dirs {
        let maybe_msvcp = redist_dir.join("msvcp140.dll");
        let maybe_vcruntime = redist_dir.join("vcruntime140.dll");
        let maybe_vcruntime_1 = redist_dir.join("vcruntime140_1.dll");
        if maybe_msvcp.exists() && maybe_vcruntime.exists() && maybe_vcruntime_1.exists() {
            res.push((maybe_msvcp, "msvcp140.dll".to_owned()));
            res.push((maybe_vcruntime, "vcruntime140.dll".to_owned()));
            res.push((maybe_vcruntime_1, "vcruntime140_1.dll".to_owned()));
            did_find_redists = true;
            break;
        }
    }

    assert!(did_find_redists, "Couldn't find MSVC redistributables");

    res
}
