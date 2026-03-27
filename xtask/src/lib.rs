// TODO: create a ytool that doesn't require huge uniffi

#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::rc::Rc;

#[cfg(target_os = "macos")]
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args;
use xshell::Shell;

pub mod android;
pub mod csharp;
pub mod kotlin;
pub mod mdns;
pub mod receiver;
pub mod sender;
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
