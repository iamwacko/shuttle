use std::fs::read_to_string;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use cargo_metadata::Message;
use cargo_metadata::{Package, Target};
use crossbeam_channel::Sender;
use shuttle_common::project::ProjectName;
use tracing::{debug, trace};

use crate::{NEXT_NAME, RUNTIME_NAME};

#[derive(Clone, Debug, Eq, PartialEq)]
/// This represents a compiled alpha or shuttle-next service.
pub struct BuiltService {
    pub executable_path: PathBuf,
    pub is_wasm: bool,
    pub package_name: String,
    pub working_directory: PathBuf,
    pub manifest_path: PathBuf,
}

impl BuiltService {
    pub fn new(
        executable_path: PathBuf,
        is_wasm: bool,
        package_name: String,
        working_directory: PathBuf,
        manifest_path: PathBuf,
    ) -> Self {
        Self {
            executable_path,
            is_wasm,
            package_name,
            working_directory,
            manifest_path,
        }
    }

    /// Try to get the service name of a crate from Shuttle.toml in the crate root, if it doesn't
    /// exist get it from the Cargo.toml package name of the crate.
    pub fn service_name(&self) -> anyhow::Result<ProjectName> {
        let shuttle_toml_path = self.working_directory.join("Shuttle.toml");

        match extract_shuttle_toml_name(shuttle_toml_path) {
            Ok(service_name) => Ok(service_name.parse()?),
            Err(error) => {
                debug!(?error, "failed to get service name from Shuttle.toml");

                // Couldn't get name from Shuttle.toml, use package name instead.
                Ok(self.package_name.parse()?)
            }
        }
    }
}

fn extract_shuttle_toml_name(path: PathBuf) -> anyhow::Result<String> {
    let shuttle_toml = read_to_string(path).context("Shuttle.toml not found")?;

    let toml: toml::Value =
        toml::from_str(&shuttle_toml).context("failed to parse Shuttle.toml")?;

    let name = toml
        .get("name")
        .context("couldn't find `name` key in Shuttle.toml")?
        .as_str()
        .context("`name` key in Shuttle.toml must be a string")?
        .to_string();

    Ok(name)
}

/// Given a project directory path, builds the crate
pub async fn build_workspace(
    project_path: &Path,
    release_mode: bool,
    _tx: Sender<Message>,
) -> anyhow::Result<Vec<BuiltService>> {
    let project_path = project_path.to_owned();

    let manifest_path = project_path.join("Cargo.toml");

    // This satisfies a test
    if !manifest_path.exists() {
        return Err(anyhow!("failed to read"));
    }
    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&manifest_path)
        .exec()?;
    trace!("Cargo metadata parsed");

    let mut alpha_packages = Vec::new();
    let mut next_packages = Vec::new();

    for member in metadata.workspace_packages() {
        println!("{}", member.name);
        if is_next(member) {
            ensure_cdylib(member)?;
            next_packages.push(member);
        } else if is_alpha(member) {
            ensure_binary(member)?;
            alpha_packages.push(member);
        }
    }

    let mut runtimes = Vec::new();

    let cwd = std::env::current_dir()?;

    if !alpha_packages.is_empty() {
        let mut compilation = compiler(
            alpha_packages,
            release_mode,
            false,
            project_path.clone(),
            cwd.clone(),
        )?;
        trace!("alpha packages compiled");

        runtimes.append(&mut compilation);
    }

    if !next_packages.is_empty() {
        let mut compilation = compiler(next_packages, release_mode, true, project_path, cwd)?;
        trace!("next packages compiled");

        runtimes.append(&mut compilation);
    }

    Ok(runtimes)
}

pub fn clean_crate(project_path: &Path, release_mode: bool) -> anyhow::Result<Vec<String>> {
    let project_path = project_path.to_owned();
    let manifest_path = project_path.join("Cargo.toml");
    let mut profile = "dev";
    if release_mode {
        profile = "release";
    }

    // It is easier just to use several pipes
    let (mut stderr_read, mut stderr_write) = pipe::pipe();
    let (mut stdout_read, mut stdout_write) = pipe::pipe();
    let (mut status_read, mut status_write) = pipe::pipe();

    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("cargo")
            .arg("clean")
            .arg("--manifest-path")
            .arg(manifest_path.to_str().unwrap())
            .arg("--profile")
            .arg(profile)
            .output()
            .unwrap();
        let mut status = "false";
        if output.clone().status.success() {
            status = "true";
        }

        stdout_write.write_all(&output.clone().stdout).unwrap();
        stderr_write.write_all(&output.stderr).unwrap();
        status_write.write_all(status.as_bytes()).unwrap();
    });

    let mut buffer = String::new();
    status_read.read_to_string(&mut buffer).unwrap();
    let mut status = false;
    if buffer == "true" {
        status = true;
    }
    let mut stderr = String::new();
    let mut stdout = String::new();
    stderr_read.read_to_string(&mut stderr)?;
    stdout_read.read_to_string(&mut stdout)?;

    if status {
        let lines = vec![stderr, stdout];
        Ok(lines)
    } else {
        Err(anyhow!("cargo clean failed"))
    }
}

fn is_next(package: &Package) -> bool {
    package
        .dependencies
        .iter()
        .any(|dependency| dependency.name == NEXT_NAME)
}

fn is_alpha(package: &Package) -> bool {
    package
        .dependencies
        .iter()
        .any(|dependency| dependency.name == RUNTIME_NAME)
}

/// Make sure the project is a binary for alpha projects.
fn ensure_binary(package: &Package) -> anyhow::Result<()> {
    if package.targets.iter().any(|target| target.is_bin()) {
        Ok(())
    } else {
        bail!("Your Shuttle project must be a binary.")
    }
}

/// Make sure "cdylib" is set for shuttle-next projects, else set it if possible.
fn ensure_cdylib(package: &Package) -> anyhow::Result<()> {
    if package.targets.iter().any(is_cdylib) {
        Ok(())
    } else {
        bail!("Your Shuttle next project must be a library. Please add `[lib]` to your Cargo.toml file.")
    }
}

fn is_cdylib(target: &Target) -> bool {
    target.kind.iter().any(|kind| kind == "cdylib")
}

fn compiler(
    packages: Vec<&Package>,
    release_mode: bool,
    wasm: bool,
    project_path: PathBuf,
    cwd: PathBuf,
) -> anyhow::Result<Vec<BuiltService>> {
    let jobs = std::thread::available_parallelism()?.get();
    let manifest_path = project_path.join("Cargo.toml");

    let mut cargo = std::process::Command::new("cargo");

    cargo
        .arg("build")
        .arg("-j")
        .arg(jobs.to_string())
        .arg("--manifest-path")
        .arg(manifest_path);

    for package in packages.clone() {
        cargo.arg("--package").arg(package.name.clone());
    }

    let mut profile = "debug";

    if release_mode {
        profile = "release";
        cargo.arg("--profile").arg("release");
    } else {
        cargo.arg("--profile").arg("dev");
    }

    if wasm {
        cargo.arg("--target").arg("wasm32-wasi");
    }

    cargo.output()?;

    let mut outputs = Vec::new();

    for package in packages.clone() {
        if wasm {
            let mut path: PathBuf = [
                project_path.clone(),
                "target".into(),
                "wasm32-wasi".into(),
                profile.into(),
                package.clone().name.into(),
            ]
            .iter()
            .collect();
            path.set_extension("wasm");

            let output = BuiltService::new(
                path.clone(),
                true,
                package.clone().name,
                cwd.clone(),
                package.clone().manifest_path.into_std_path_buf(),
            );

            outputs.push(output);
        } else {
            let mut path: PathBuf = [
                project_path.clone(),
                "target".into(),
                profile.into(),
                package.clone().name.into(),
            ]
            .iter()
            .collect();
            path.set_extension(std::env::consts::EXE_SUFFIX);
            let output = BuiltService::new(
                path.clone(),
                false,
                package.clone().name,
                cwd.clone(),
                package.clone().manifest_path.into_std_path_buf(),
            );

            outputs.push(output);
        }
    }

    Ok(outputs)
}
