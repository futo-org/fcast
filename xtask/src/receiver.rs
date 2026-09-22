use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Args, Subcommand};
use xshell::cmd;

#[cfg(target_os = "macos")]
use crate::BuildMacosInstallerArgs;
use crate::{sh, workspace, AndroidAbiTarget};

#[cfg(target_os = "macos")]
#[derive(askama::Template)]
#[template(path = "receiver.Info.plist.askama")]
struct InfoPlistTemplate {
    version: String,
}

#[cfg(target_os = "windows")]
#[derive(askama::Template)]
#[template(path = "receiver.Product.wxs.askama", escape = "none")]
struct ProductTemplate {
    version: String,
    dll_components: String,
}

#[derive(Subcommand)]
pub enum AndroidReceiverCommand {
    Check,
    Clippy,
    Build {
        #[clap(short, long)]
        release: bool,
        #[clap(short, long)]
        target: Option<AndroidAbiTarget>,
    },
    /// Build the native libraries and package them with gradle.
    Package(PackageArgs),
}

#[derive(Args)]
pub struct PackageArgs {
    #[clap(short, long)]
    pub release: bool,
    /// ABIs to package, repeatable. Defaults to the two that ship.
    #[clap(short, long)]
    pub target: Vec<AndroidAbiTarget>,
    /// Build the playstore aab instead of the sideload apk.
    #[clap(long)]
    pub bundle: bool,
    /// Play's update ordinal. A store upload must exceed the live listing's.
    #[clap(long, default_value_t = 1)]
    pub version_code: u32,
    #[clap(long, default_value = "1.0")]
    pub version_name: String,
    /// Package the jniLibs already in the tree instead of rebuilding them.
    #[clap(long)]
    pub skip_native: bool,
}

#[derive(Args)]
pub struct AndroidReceiverArgs {
    #[clap(subcommand)]
    pub cmd: AndroidReceiverCommand,
    #[clap(long)]
    pub android_home_override: Option<String>,
    #[clap(long)]
    pub android_ndk_root_override: Option<String>,
}

#[derive(Subcommand)]
pub enum ReceiverCommand {
    Android(AndroidReceiverArgs),
    /// Build the desktop receiver.
    BuildStatic(CargoSubcmdArgs),
    /// Build the receiver and run it. Arguments after `--` are forwarded to
    /// the receiver binary.
    Run(RunStaticArgs),
    /// `cargo check` the desktop receiver.
    Check(CargoSubcmdArgs),
    /// `cargo clippy` the desktop receiver.
    Clippy(CargoSubcmdArgs),
    /// `cargo test` receiver-core. Args after `--` go to the libtest harness,
    /// e.g. `-- --nocapture` or a test-name filter.
    Test(CargoSubcmdArgs),
    #[cfg(target_os = "windows")]
    BuildWindowsInstaller(crate::gstreamer::GstreamerArgs),
    #[cfg(target_os = "macos")]
    BuildMacosInstaller(BuildMacosInstallerArgs),
}

#[derive(Args)]
pub struct CargoSubcmdArgs {
    /// Use the release profile instead of the default fast debug build.
    #[arg(long)]
    pub release: bool,
    /// Extra args appended to the inner cargo invocation (everything after
    /// `--`).
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct RunStaticArgs {
    /// Build the receiver in release.
    #[arg(long)]
    pub release: bool,
    /// Arguments forwarded to the receiver binary (everything after `--`).
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args)]
pub struct ReceiverArgs {
    #[clap(subcommand)]
    pub cmd: ReceiverCommand,
}

/// GStreamer is built and linked by the gstreamer-src crate, so these commands
/// are thin `cargo` wrappers, kept for the scripts that still call them.
fn cargo_receiver(
    sh: &xshell::Shell,
    subcmd: &str,
    package: &str,
    release: bool,
    extra: &[String],
) -> Result<()> {
    let mut args: Vec<String> = vec![subcmd.to_owned(), "-p".to_owned(), package.to_owned()];
    if release {
        args.push("--release".to_owned());
    }
    if !extra.is_empty() {
        args.push("--".to_owned());
        args.extend(extra.iter().cloned());
    }
    cmd!(sh, "cargo {args...}").run()?;
    Ok(())
}

/// The receiver ships arm only: x86_64 is emulator territory, which a cast
/// target is not, and x86 has no linker in .cargo/config.toml at all. The
/// sender lane still builds all four, which is why the enum keeps them.
fn reject_emulator_abis(targets: &[AndroidAbiTarget]) -> Result<()> {
    match targets
        .iter()
        .find(|t| matches!(t, AndroidAbiTarget::X64 | AndroidAbiTarget::X86))
    {
        Some(bad) => anyhow::bail!(
            "{} is not a receiver ABI, the apk ships arm64 and armv7",
            bad.translate()
        ),
        None => Ok(()),
    }
}

/// cargo-ndk per ABI, straight into the gradle source set: jniLibs is where
/// packaging picks the libraries up and where the 16KB alignment assert looks.
fn build_android_native(
    sh: &xshell::Shell,
    root_path: &Utf8PathBuf,
    release: bool,
    targets: &[AndroidAbiTarget],
) -> Result<()> {
    reject_emulator_abis(targets)?;

    let out_dir = concat_path(
        root_path,
        "receivers/experimental/android/app/src/main/jniLibs",
    );

    for target in targets {
        let target = target.translate();

        let mut args = vec![
            "--target",
            target,
            "-o",
            out_dir.as_str(),
            "build",
            "--package",
            "receiver-android",
        ];
        if release {
            args.push("--release");
        }

        cmd!(sh, "cargo ndk {args...}").run()?;
    }

    Ok(())
}

/// What the variant directory holds, walked rather than assumed: AGP names
/// the file from the flavor and the build type, and an up-to-date build is
/// still a build that produced one.
fn report_packages(outputs: &Utf8PathBuf) -> Result<()> {
    let mut found = Vec::new();
    let mut stack = vec![outputs.clone()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !matches!(path.extension(), Some("apk" | "aab")) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            found.push((path, meta.len()));
        }
    }

    if found.is_empty() {
        anyhow::bail!("gradle wrote no apk or aab under {outputs}");
    }

    found.sort();
    for (path, size) in found {
        println!(">> {path} ({:.1} MB)", size as f64 / 1_048_576.0);
    }

    Ok(())
}

/// The platform jar the java glue compiles against, from the SDK `xtask
/// android download-sdk` provisions. One pinned API level, so this lane and
/// gradle's compileSdk cannot drift apart.
fn android_jar(sh: &xshell::Shell) -> Result<Utf8PathBuf> {
    let home = Utf8PathBuf::from(sh.var("ANDROID_HOME")?);
    let jar = concat_path(
        &home,
        &format!(
            "platforms/{}/android.jar",
            crate::android::ANDROID_PLATFORM
        ),
    );
    if !jar.is_file() {
        anyhow::bail!("no {jar}: install it with `cargo xtask android download-sdk`");
    }
    Ok(jar)
}

/// JAVA_HOME for the android lanes: the environment's when its javac runs,
/// else the JDK around the javac on PATH. Checked by running it, because a
/// javac that cannot start (a JDK from one nixpkgs against the devshell's
/// glibc, say) fails exactly like having none.
fn resolve_java_home() -> Result<Utf8PathBuf> {
    let home = match std::env::var("JAVA_HOME") {
        Ok(h) if !h.is_empty() => Utf8PathBuf::from(h),
        _ => javac_on_path().ok_or_else(|| {
            anyhow::anyhow!(
                "no JDK for the android lane, which compiles java glue. Set JAVA_HOME \
                 or put javac on PATH; the devshell ships one."
            )
        })?,
    };

    let javac = concat_path(&home, "bin/javac");
    if !javac.is_file() {
        anyhow::bail!("JAVA_HOME is {home}, which has no bin/javac");
    }
    let probe = std::process::Command::new(&javac)
        .arg("-version")
        .output()
        .map_err(|e| anyhow::anyhow!("{javac} did not start: {e}"))?;
    if !probe.status.success() {
        anyhow::bail!(
            "{javac} does not run: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }

    Ok(home)
}

/// The JDK around the first javac on PATH, symlinks resolved so the home is
/// the real one and not /usr.
fn javac_on_path() -> Option<Utf8PathBuf> {
    let javac = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("javac"))
        .find(|javac| javac.is_file())?;
    let javac = std::fs::canonicalize(&javac).unwrap_or(javac);
    let home = Utf8PathBuf::from_path_buf(javac).ok()?;
    Some(home.parent()?.parent()?.to_owned())
}

fn concat_path(a: &Utf8PathBuf, b: &str) -> Utf8PathBuf {
    let mut res = a.clone();
    res.push(b);
    res
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn get_receiver_version() -> String {
    let receiver_toml = std::fs::read_to_string("receivers/desktop/Cargo.toml").unwrap();
    let doc = receiver_toml.parse::<toml_edit::DocumentMut>().unwrap();
    doc["package"]["version"].as_str().unwrap().to_string()
}

impl ReceiverArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root_path = workspace::root_path()?;
        // Run from the workspace root so the receiver build's relative paths resolve
        // even when invoked from a subdirectory (rust-analyzer / rustic). Both chdir
        // and push_dir: the latter misses the std::fs / Command calls.
        std::env::set_current_dir(&root_path)
            .map_err(|e| anyhow::anyhow!("chdir to workspace root {root_path}: {e}"))?;
        let _p = sh.push_dir(root_path.clone());

        match self.cmd {
            ReceiverCommand::BuildStatic(a) => {
                crate::gstreamer::guard_receiver_relink(a.release)?;
                return cargo_receiver(&sh, "build", "desktop-receiver", a.release, &a.args);
            }
            ReceiverCommand::Run(a) => {
                crate::gstreamer::guard_receiver_relink(a.release)?;
                return cargo_receiver(&sh, "run", "desktop-receiver", a.release, &a.args);
            }
            ReceiverCommand::Check(a) => {
                return cargo_receiver(&sh, "check", "desktop-receiver", a.release, &a.args);
            }
            ReceiverCommand::Clippy(a) => {
                return cargo_receiver(&sh, "clippy", "desktop-receiver", a.release, &a.args);
            }
            ReceiverCommand::Test(a) => {
                return cargo_receiver(&sh, "test", "receiver-core", a.release, &a.args);
            }
            ReceiverCommand::Android(args) => {
                let _env_andr_sdk = sh.push_env(
                    "ANDROID_HOME",
                    concat_path(
                        &root_path,
                        &args
                            .android_home_override
                            .unwrap_or(crate::android::ANDROID_HOME_PATH.to_owned()),
                    ),
                );
                let _env_ndk = sh.push_env(
                    "ANDROID_NDK_ROOT",
                    concat_path(
                        &root_path,
                        &args
                            .android_ndk_root_override
                            .clone()
                            .unwrap_or(crate::android::NDK_PATH.to_owned()),
                    ),
                );
                // Needed for some skia stuff on some arm target
                let _env_andr_ndk = sh.push_env(
                    "ANDROID_NDK",
                    concat_path(
                        &root_path,
                        &args
                            .android_ndk_root_override
                            .unwrap_or(crate::android::NDK_PATH.to_owned()),
                    ),
                );
                let _env_pkg_config_cross = sh.push_env("PKG_CONFIG_ALLOW_CROSS", "1");
                // Without this cargo-ndk links against its own default sysroot,
                // and the link fails on the libraries that arrived after it.
                let _env_api = sh.push_env(
                    "CARGO_NDK_PLATFORM",
                    crate::android::ANDROID_API.to_string(),
                );

                // The android-activity backend compiles java glue in its build
                // script, which needs a JDK and the platform jar. Both resolved
                // here for the lanes that reach it: a missing or unusable one
                // otherwise surfaces as a panic three crates deep in someone
                // else's build.rs.
                let _env_java = if matches!(
                    args.cmd,
                    AndroidReceiverCommand::Check
                        | AndroidReceiverCommand::Clippy
                        | AndroidReceiverCommand::Build { .. }
                        | AndroidReceiverCommand::Package(_)
                ) {
                    Some((
                        sh.push_env("JAVA_HOME", resolve_java_home()?),
                        sh.push_env("ANDROID_JAR", android_jar(&sh)?),
                    ))
                } else {
                    None
                };

                match args.cmd {
                    AndroidReceiverCommand::Check => {
                        cmd!(
                            sh,
                            "cargo ndk --target aarch64-linux-android check -p receiver-android"
                        )
                        .run()?
                    }
                    AndroidReceiverCommand::Clippy => {
                        cmd!(
                            sh,
                            "cargo ndk --target aarch64-linux-android clippy -p receiver-android"
                        )
                        .run()?
                    }
                    AndroidReceiverCommand::Build { release, target } => {
                        let targets = target
                            .map(|t| vec![t])
                            .unwrap_or(vec![AndroidAbiTarget::Arm64, AndroidAbiTarget::Arm32]);
                        build_android_native(&sh, &root_path, release, &targets)?;
                    }
                    AndroidReceiverCommand::Package(p) => {
                        // The two ABIs the apk ships, the only two the native
                        // build accepts.
                        let targets = if p.target.is_empty() {
                            vec![AndroidAbiTarget::Arm64, AndroidAbiTarget::Arm32]
                        } else {
                            p.target.clone()
                        };
                        reject_emulator_abis(&targets)?;
                        if !p.skip_native {
                            build_android_native(&sh, &root_path, p.release, &targets)?;
                        }

                        let project = concat_path(&root_path, "receivers/experimental/android");
                        // Gradle reads local.properties before ANDROID_HOME, and that
                        // file is a Studio artifact naming whatever SDK the machine
                        // happens to have. Written from the provisioned one, so the
                        // package is built against the same platform the native lane
                        // compiled its java glue against.
                        let sdk = sh.var("ANDROID_HOME")?;
                        let props = concat_path(&project, "local.properties");
                        sh.write_file(&props, format!("sdk.dir={sdk}\n"))?;
                        println!(">> {props} points at {sdk}");

                        // The gradle task and the directory AGP writes it to, which
                        // is named after the same variant.
                        let (task, outputs) = match (p.bundle, p.release) {
                            (true, true) => ("bundlePlaystoreRelease", "bundle/playstoreRelease"),
                            (true, false) => ("bundlePlaystoreDebug", "bundle/playstoreDebug"),
                            (false, true) => {
                                ("assembleDefaultFlavorRelease", "apk/defaultFlavor/release")
                            }
                            (false, false) => {
                                ("assembleDefaultFlavorDebug", "apk/defaultFlavor/debug")
                            }
                        };
                        let version_code = p.version_code.to_string();
                        let version_name = p.version_name;

                        {
                            let _dir = sh.push_dir(&project);
                            cmd!(
                                sh,
                                "./gradlew --stacktrace {task}
                                 -PversionCode={version_code} -PversionName={version_name}"
                            )
                            .run()?;
                        }

                        report_packages(&concat_path(
                            &project,
                            &format!("app/build/outputs/{outputs}"),
                        ))?;
                    }
                }
            }
            #[cfg(target_os = "windows")]
            ReceiverCommand::BuildWindowsInstaller(static_args) => {
                // scope=Full (the mac/win default): gstreamer, the glib/pango stack,
                // codecs and the GIO TLS module are ALL statically linked, no dev kit
                // and no bundled DLLs beyond the MSVC redists. NEEDS VALIDATION on a
                // Windows box: `dumpbin /dependents` must show only OS DLLs + redists.
                let binary = static_args.build()?;

                let build_dir_root = crate::setup_build_dir(&sh, &root_path);

                let mut files_to_copy = Vec::new();
                files_to_copy.push((
                    concat_path(&root_path, binary.as_str()),
                    "fcast-receiver.exe".to_string(),
                ));

                files_to_copy.extend(crate::find_msvc_redists(&sh));
                files_to_copy.extend(crate::find_c_runtime(
                    crate::find_windows_sdk_installation_path(),
                ));
                files_to_copy.push(("receivers/extra/fcast.ico".into(), "fcast.ico".to_owned()));

                let mut dll_components = String::new();

                for (src, dst) in files_to_copy {
                    let dst = concat_path(&build_dir_root, &dst);
                    sh.copy_file(&src, &dst)?;
                    println!("Copied `{src}` to `{dst}`");

                    if dst.extension() == Some("dll") {
                        dll_components += &format!(r#"<File Source="{dst}" />"#);
                        dll_components += "\n";
                    }
                }

                use askama::Template;

                let receiver_version = get_receiver_version();
                let product_wxs = ProductTemplate {
                    version: receiver_version.clone(),
                    dll_components,
                }
                .render()?;

                sh.write_file(
                    concat_path(&build_dir_root, &"FCastReceiverInstaller.wxs"),
                    product_wxs,
                )?;

                println!("############### Building installer ###############");

                {
                    let output = format!("FCastReceiver-{receiver_version}-win64-installer.msi");
                    let _win_build_p = sh.push_dir(&build_dir_root);
                    cmd!(sh, "wix build -out {output} .\\FCastReceiverInstaller.wxs").run()?;
                }
            }
            #[cfg(target_os = "macos")]
            ReceiverCommand::BuildMacosInstaller(BuildMacosInstallerArgs {
                sign,
                p12_file,
                p12_password_file,
                api_key_file,
                static_args,
            }) => {
                let path_to_dmg_dir = root_path.join("target").join("fcast-receiver-dmg");
                let app_top_level = path_to_dmg_dir.join("FCast Receiver.app");
                let build_dir_root = app_top_level.join("Contents").join("MacOS");

                if sh.remove_path(&path_to_dmg_dir).is_ok() {
                    println!("Removed old build dir at `{path_to_dmg_dir:?}`")
                }

                sh.create_dir(&build_dir_root)?;

                // scope=Full (the macOS default): gstreamer, the glib/pango stack,
                // codecs and the GIO TLS module are ALL static, no framework dev kit,
                // no dylib bundling. Anything non-OS left dynamic is a failure.
                // Default features stay on so the system tray is built in (its
                // macOS impl is NSStatusItem via the slint fork — no extra dylib).
                let binary = static_args.build()?;
                let binary_path = concat_path(&root_path, binary.as_str());

                let leftover = crate::find_non_system_dependencies_with_otool(&binary_path);
                if !leftover.is_empty() {
                    anyhow::bail!(
                        "static build still links non-system dylibs: {leftover:?}\n\
                         These would dangle on user machines. Fix the static build \
                         instead of bundling them."
                    );
                }

                std::fs::copy(&binary_path, build_dir_root.join("fcast-receiver"))?;

                use askama::Template;

                println!("############### Writing resources ###############");

                let receiver_version = get_receiver_version();
                let info_plist = InfoPlistTemplate {
                    version: receiver_version.clone(),
                }
                .render()?;
                sh.create_dir(app_top_level.join("Contents").join("Resources"))?;
                sh.copy_file(
                    root_path.join("receivers").join("extra").join("fcast.icns"),
                    app_top_level
                        .join("Contents")
                        .join("Resources")
                        .join("fcast.icns"),
                )?;
                sh.write_file(
                    app_top_level.join("Contents").join("Info.plist"),
                    info_plist,
                )?;
                let applications_link_path = path_to_dmg_dir.join("Applications");
                let path_to_dmg = root_path.join("target").join(format!(
                    "fcast-receiver-{receiver_version}-macos-aarch64.dmg"
                ));
                sh.remove_path(&path_to_dmg)?;

                crate::create_package(
                    &sh,
                    crate::AppType::Receiver,
                    receiver_version,
                    app_top_level,
                    applications_link_path,
                    path_to_dmg,
                    path_to_dmg_dir,
                    sign,
                    p12_file,
                    p12_password_file,
                    api_key_file,
                );
            }
        }

        Ok(())
    }
}
