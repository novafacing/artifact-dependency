// Copyright (C) 2023 Intel Corporation, Rowan Hart
// SPDX-License-Identifier: Apache-2.0

//! Feature-light crate to build and use dependencies whose results are Artifacts:
//! - Static Libraries
//! - C Dynamic Libraries
//! - Binaries

#![deny(clippy::unwrap_used)]
#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, ensure, Result};
use cargo_metadata::{camino::Utf8PathBuf, MetadataCommand, Package};
use serde::{Deserialize, Serialize};
use std::{
    env::var,
    hash::{Hash, Hasher},
    path::PathBuf,
    process::{Command, Stdio},
    str::FromStr,
};
use typed_builder::TypedBuilder;

#[derive(Clone, Debug, Copy)]
/// Crate type to include as the built [`Artifact`]
pub enum CrateType {
    Executable,
    CDynamicLibrary,
    Dylib,
    StaticLibrary,
    RustLibrary,
    ProcMacro,
    // NOTE: Doesn't include raw-dylib, which allows DLL linking without import libraries:
    // https://rust-lang.github.io/rfcs/2627-raw-dylib-kind.html
}

#[derive(Clone, Debug)]
/// Profile to build. [`ArtifactDependency`] defaults to building the current profile in use,
/// but a different profile can be selected.
pub enum Profile {
    Release,
    Dev,
    Other(String),
}

impl FromStr for Profile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "release" => Ok(Self::Release),
            "dev" => Ok(Self::Dev),
            "debug" => Ok(Self::Dev),
            _ => Ok(Self::Other(s.to_string())),
        }
    }
}

#[derive(TypedBuilder, Clone, Debug)]
/// Builder to find and optionally build an artifact dependency from a particular workspace
pub struct ArtifactDependency {
    #[builder(setter(into, strip_option), default)]
    /// Workspace root to search for an artifact dependency in. Defaults to the current workspace
    /// if one is not provided.
    pub workspace_root: Option<PathBuf>,
    /// Crate name to search for an artifact dependency for. Defaults to `CARGO_PKG_NAME` if it
    /// is set, otherwise the root package name in the workspace. If neither are set, an error
    /// will be returned.
    #[builder(setter(into, strip_option), default)]
    pub crate_name: Option<String>,
    /// Type of artifact to build.
    pub artifact_type: CrateType,
    #[builder(setter(into))]
    /// Profile, defaults to the current profile
    pub profile: Profile,
    /// Build the artifact if it is missing
    pub build_missing: bool,
    #[builder(default = true)]
    /// (Re-)build the artifact even if it is not missing. This is the default because otherwise
    /// it's very common to have a "what is going on why aren't my print statements showing up"
    /// moment
    pub build_always: bool,
    #[builder(setter(into), default)]
    pub features: Vec<String>,
    #[builder(setter(into, strip_option), default)]
    pub target_name: Option<String>,
    #[builder(setter(into), default)]
    pub capture_output: bool,
    #[builder(setter(into, strip_option), default)]
    pub env: Option<Vec<(String, String)>>,
}

// NOTE: Artifact naming is not very easy to discern, we have to dig hard into rustc.
// Windows dll import lib: https://github.com/rust-lang/rust/blob/b2b34bd83192c3d16c88655158f7d8d612513e88/compiler/rustc_codegen_llvm/src/back/archive.rs#L129
// Others by crate type: https://github.com/rust-lang/rust/blob/b2b34bd83192c3d16c88655158f7d8d612513e88/compiler/rustc_session/src/output.rs#L141
// The default settings: https://github.com/rust-lang/rust/blob/db9d1b20bba1968c1ec1fc49616d4742c1725b4b/compiler/rustc_target/src/spec/mod.rs#L1422-L1529
//
// | Platform Spec   | DLL Prefix | DLL Suffix | EXE Suffix | Staticlib Prefix | Staticlib Suffix |
// | Default         | lib (d)    | .so (d)    |            | lib (d)          | .a (d)           |
// | MSVC            |            | .dll       | .exe       |                  | .lib             |
// | Windows GNU     |            | .dll       | .exe       | lib (d)          | .a (d)           |
// | WASM            | lib (d)    | .wasm      | .wasm      | lib (d)          | .a (d)           |
// | AIX             | lib (d)    | .a         |            | lib (d)          | .a (d)           |
// | Apple           | lib (d)    | .dylib     |            | lib (d)          | .a (d,framework?)|
// | NVPTX           |            | .ptx       | .ptx       | lib (d)          | .a (d)           |
// | Windows GNULLVM |            | .dll       | .exe       | lib (d)          | .a (d)           |

#[cfg(target_family = "unix")]
/// The dll prefix, dll suffix, staticlib prefix, staticlib suffix, and exe suffix for the current target
pub const ARTIFACT_NAMEPARTS: (&str, &str, &str, &str, &str) = ("lib", ".so", "lib", ".a", "");
#[cfg(target_family = "darwin")]
/// The dll prefix, dll suffix, staticlib prefix, staticlib suffix, and exe suffix for the current target
pub const ARTIFACT_NAMEPARTS: (&str, &str, &str, &str, &str) = ("lib", ".dylib", "lib", ".a", "");
#[cfg(all(target_os = "windows", target_env = "msvc"))]
/// The dll prefix, dll suffix, staticlib prefix, staticlib suffix, and exe suffix for the current target
pub const ARTIFACT_NAMEPARTS: (&str, &str, &str, &str, &str) = ("", ".dll", "", ".lib", ".exe");
#[cfg(all(target_os = "windows", any(target_env = "gnu", target_env = "gnullvm")))]
/// The dll prefix, dll suffix, staticlib prefix, staticlib suffix, and exe suffix for the current target
pub const ARTIFACT_NAMEPARTS: (&str, &str, &str, &str, &str) = ("", ".dll", "lib", ".a", ".exe");

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
/// A built artifact
pub struct Artifact {
    /// The path to the artifact output, as specified by the `artifact_type` field if the
    /// dependency has multiple outputs.
    pub path: PathBuf,
    /// Package metadata for the artifact
    pub package: Package,
}

impl Hash for Artifact {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.path.hash(state);
        self.package.name.hash(state);
        self.package.version.hash(state);
        self.package.authors.hash(state);
        self.package.id.hash(state);
        self.package.description.hash(state);
        self.package.license.hash(state);
        self.package.license_file.hash(state);
        self.package.targets.hash(state);
        self.package.manifest_path.hash(state);
        self.package.categories.hash(state);
        self.package.keywords.hash(state);
        self.package.readme.hash(state);
        self.package.repository.hash(state);
        self.package.homepage.hash(state);
        self.package.documentation.hash(state);
        self.package.edition.hash(state);
        self.package.links.hash(state);
        self.package.publish.hash(state);
        self.package.default_run.hash(state);
        self.package.rust_version.hash(state);
    }
}

impl Artifact {
    /// Instantiate a new artifact at a path with a given metadata object
    fn new(path: PathBuf, package: Package) -> Self {
        Self { path, package }
    }
}

impl ArtifactDependency {
    /// Build the dependency by invoking `cargo build`
    pub fn build(&mut self) -> Result<Artifact> {
        let workspace_root = if let Some(workspace_root) = self.workspace_root.clone() {
            workspace_root
        } else {
            MetadataCommand::new()
                .no_deps()
                .exec()?
                .workspace_root
                .into()
        };

        let metadata = MetadataCommand::new()
            .current_dir(&workspace_root)
            .no_deps()
            .manifest_path(workspace_root.join("Cargo.toml"))
            .exec()?;

        self.crate_name = if let Some(crate_name) = self.crate_name.as_ref() {
            Some(crate_name.clone())
        } else if let Ok(crate_name) = var("CARGO_PKG_NAME") {
            Some(crate_name)
        } else if let Some(root_package) = metadata.root_package() {
            Some(root_package.name.clone())
        } else {
            bail!("No name provided and no root package in provided workspace at {}, could not determine crate name.", workspace_root.display());
        };

        let crate_name = self
            .crate_name
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("self.crate_name must have a value at this point"))?;

        let package = metadata
            .packages
            .iter()
            .find(|p| p.name == crate_name)
            .ok_or_else(|| {
                anyhow!(
                    "No package matching name {} found in packages {:?} workspace at {}",
                    crate_name,
                    metadata
                        .packages
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>(),
                    workspace_root.display()
                )
            })?;

        let package_name = package.name.clone();
        let package_result_name = package_name.replace('-', "_");

        let (dll_prefix, dll_suffix, staticlib_prefix, staticlib_suffix, exe_suffix) =
            ARTIFACT_NAMEPARTS;

        let profile = self.profile.clone();

        let profile_target_path = metadata.target_directory.join(match &profile {
            Profile::Release => "release".to_string(),
            Profile::Dev => "debug".to_string(),
            Profile::Other(o) => o.clone(),
        });

        let artifact_path = match self.artifact_type {
            CrateType::Executable => {
                profile_target_path.join(format!("{}{}", &package_result_name, exe_suffix))
            }
            CrateType::CDynamicLibrary => profile_target_path.join(format!(
                "{}{}{}",
                dll_prefix, &package_result_name, dll_suffix
            )),
            CrateType::StaticLibrary => profile_target_path.join(format!(
                "{}{}{}",
                staticlib_prefix, package_result_name, staticlib_suffix
            )),
            _ => bail!(
                "Crate type {:?} is not supported as an artifact dependency source",
                self.artifact_type
            ),
        };

        let artifact_path = if (self.build_missing && !artifact_path.exists()) || self.build_always
        {
            let cargo = var("CARGO").unwrap_or("cargo".to_string());
            let mut cargo_command = Command::new(cargo);
            cargo_command
                .arg("build")
                .arg("--manifest-path")
                .arg(workspace_root.join("Cargo.toml"))
                .arg("--package")
                .arg(&package_name);

            // TODO: This will solve one build script trying to build the artifact at
            // once, but doesn't resolve parallel scripts trying to both build it
            // simultaneously, we need to actually detect the lock.
            let build_target_dir = if let Some(target_name) = self.target_name.as_ref() {
                metadata.target_directory.join(target_name)
            } else {
                metadata.target_directory
            };

            cargo_command.arg("--target-dir").arg(&build_target_dir);

            match &profile {
                Profile::Release => {
                    cargo_command.arg("--release");
                }
                Profile::Other(o) => {
                    cargo_command.args(vec!["--profile".to_string(), o.clone()]);
                }
                _ => {}
            }

            cargo_command.arg(format!("--features={}", self.features.join(",")));

            if let Some(env) = self.env.as_ref() {
                cargo_command.envs(env.iter().cloned());
            }

            if self.capture_output {
                let output = cargo_command
                    .stderr(Stdio::piped())
                    .stdout(Stdio::piped())
                    .output()?;

                if !output.status.success() {
                    bail!(
                        "Failed to build artifact crate:\nstdout: {}\nstderr: {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            } else {
                let status = cargo_command.status()?;

                if !status.success() {
                    bail!("Failed to build artifact crate");
                }
            }

            let artifact_path: PathBuf = build_target_dir
                .join({
                    let components = artifact_path
                        .components()
                        .rev()
                        .take(2)
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>();
                    components.iter().rev().collect::<Utf8PathBuf>()
                })
                .into();

            ensure!(
                artifact_path.exists(),
                "Artifact build succeeded, but artifact not found in {}",
                artifact_path.display()
            );

            artifact_path
        } else {
            artifact_path.into()
        };

        Ok(Artifact::new(artifact_path, package.clone()))
    }
}
