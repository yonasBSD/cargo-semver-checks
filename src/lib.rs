#![forbid(unsafe_code)]

mod callbacks;
mod check_release;
mod config;
mod data_generation;
mod manifest;
mod query;
mod rustdoc_gen;
mod templating;
mod util;
mod witness_gen;

use anyhow::Context;
use cargo_metadata::PackageId;
use clap::ValueEnum;
use data_generation::{DataStorage, IntoTerminalResult as _, TerminalError};
use directories::ProjectDirs;
use itertools::Itertools;
use serde::Serialize;

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use check_release::run_check_release;
use rustdoc_gen::CrateDataForRustdoc;

pub use config::{FeatureFlag, GlobalConfig};
pub use query::{
    ActualSemverUpdate, LintLevel, OverrideMap, OverrideStack, QueryOverride, RequiredSemverUpdate,
    SemverQuery, Witness,
};

/// Test a release for semver violations.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Which packages to analyze.
    scope: Scope,
    current: Rustdoc,
    baseline: Rustdoc,
    release_type: Option<ReleaseType>,
    current_feature_config: rustdoc_gen::FeatureConfig,
    baseline_feature_config: rustdoc_gen::FeatureConfig,
    /// Which `--target` to use, if unset pass no flag
    build_target: Option<String>,
    /// Options for generating [witnesses](Witness).
    witness_generation: WitnessGeneration,
}

/// The kind of release we're making.
///
/// Affects which lints are executed.
/// Non-exhaustive in case we want to add "pre-release" as an option in the future.
#[non_exhaustive]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReleaseType {
    Major,
    Minor,
    Patch,
}

#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Rustdoc {
    source: RustdocSource,
}

impl Rustdoc {
    /// Use an existing rustdoc file.
    pub fn from_path(rustdoc_path: impl Into<PathBuf>) -> Self {
        Self {
            source: RustdocSource::Rustdoc(rustdoc_path.into()),
        }
    }

    /// Generate the rustdoc file from the project root directory,
    /// i.e. the directory containing the crate source.
    /// It can be a workspace or a single package.
    /// Same as [`Rustdoc::from_git_revision()`], but with the current git revision.
    pub fn from_root(project_root: impl Into<PathBuf>) -> Self {
        Self {
            source: RustdocSource::Root(project_root.into()),
        }
    }

    /// Generate the rustdoc file from the project at a given git revision.
    pub fn from_git_revision(
        project_root: impl Into<PathBuf>,
        revision: impl Into<String>,
    ) -> Self {
        Self {
            source: RustdocSource::Revision(project_root.into(), revision.into()),
        }
    }

    /// Generate the rustdoc file from the largest-numbered non-yanked non-prerelease version
    /// published to the cargo registry. If no such version, uses
    /// the largest-numbered version including yanked and prerelease versions.
    pub fn from_registry_latest_crate_version() -> Self {
        Self {
            source: RustdocSource::VersionFromRegistry(None),
        }
    }

    /// Generate the rustdoc file from a specific crate version.
    pub fn from_registry(crate_version: impl Into<String>) -> Self {
        Self {
            source: RustdocSource::VersionFromRegistry(Some(crate_version.into())),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
enum RustdocSource {
    /// Path to the Rustdoc json file.
    /// Use this option when you have already generated the rustdoc file.
    Rustdoc(PathBuf),
    /// Project root directory, i.e. the directory containing the crate source.
    /// It can be a workspace or a single package.
    Root(PathBuf),
    /// Project root directory and Git Revision.
    Revision(PathBuf, String),
    /// Version from cargo registry to lookup. E.g. "1.0.0".
    /// If `None`, uses the largest-numbered non-yanked non-prerelease version
    /// published to the cargo registry. If no such version, uses
    /// the largest-numbered version including yanked and prerelease versions.
    VersionFromRegistry(Option<String>),
}

/// Which packages to analyze.
#[derive(Default, Debug, PartialEq, Eq, Serialize)]
struct Scope {
    mode: ScopeMode,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
enum ScopeMode {
    /// All packages except the excluded ones.
    DenyList(PackageSelection),
    /// Packages to process (see `cargo help pkgid`)
    AllowList(Vec<String>),
}

impl Default for ScopeMode {
    fn default() -> Self {
        Self::DenyList(PackageSelection::default())
    }
}

#[non_exhaustive]
#[derive(Default, Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackageSelection {
    selection: ScopeSelection,
    excluded_packages: Vec<String>,
}

impl PackageSelection {
    pub fn new(selection: ScopeSelection) -> Self {
        Self {
            selection,
            excluded_packages: vec![],
        }
    }

    pub fn set_excluded_packages(&mut self, packages: Vec<String>) -> &mut Self {
        self.excluded_packages = packages;
        self
    }
}

#[non_exhaustive]
#[derive(Default, Debug, PartialEq, Eq, Clone, Serialize)]
pub enum ScopeSelection {
    /// All packages in the workspace. Equivalent to `--workspace`.
    Workspace,
    /// Default members of the workspace.
    #[default]
    DefaultMembers,
}

impl Scope {
    /// Returns `(selected, skipped)` packages
    fn selected_packages<'m>(
        &self,
        meta: &'m cargo_metadata::Metadata,
    ) -> (
        Vec<&'m cargo_metadata::Package>,
        Vec<&'m cargo_metadata::Package>,
    ) {
        let workspace_members: HashSet<&PackageId> = meta.workspace_members.iter().collect();
        let base_ids: HashSet<&PackageId> = match &self.mode {
            ScopeMode::DenyList(PackageSelection {
                selection,
                excluded_packages,
            }) => {
                let packages = match selection {
                    ScopeSelection::Workspace => workspace_members,
                    ScopeSelection::DefaultMembers => {
                        // Deviating from cargo because Metadata doesn't have default members
                        let resolve = meta.resolve.as_ref().expect("no-deps is unsupported");
                        match &resolve.root {
                            Some(root) => {
                                let mut base_ids = HashSet::new();
                                base_ids.insert(root);
                                base_ids
                            }
                            None => workspace_members,
                        }
                    }
                };

                packages
                    .iter()
                    .filter(|p| !excluded_packages.contains(&meta[p].name))
                    .copied()
                    .collect()
            }
            ScopeMode::AllowList(patterns) => {
                meta.packages
                    .iter()
                    // Deviating from cargo by not supporting patterns
                    // Deviating from cargo by only checking workspace members
                    .filter(|p| workspace_members.contains(&p.id) && patterns.contains(&p.name))
                    .map(|p| &p.id)
                    .collect()
            }
        };

        meta.packages
            .iter()
            .filter(|&p| {
                // The package has to not have been explicitly excluded
                base_ids.contains(&p.id)
            })
            .partition(|&p| p.targets.iter().any(is_lib_like_checkable_target))
    }
}

struct CrateToCheck<'a> {
    overrides: OverrideStack,
    current_crate_data: CrateDataForRustdoc<'a>,
    baseline_crate_data: CrateDataForRustdoc<'a>,
}

/// Is the specified target able to be semver-checked as a library, of any sort.
///
/// This is a broader definition than cargo's own "lib" definition, since we can also
/// semver-check rlib, dylib, and staticlib targets as well.
#[expect(
    clippy::unneeded_struct_pattern,
    reason = "we don't want a breaking change if the target variants change from unit variants to a different kind"
)]
fn is_lib_like_checkable_target(target: &cargo_metadata::Target) -> bool {
    target.is_lib()
        || target.kind.iter().any(|kind| {
            matches!(
                kind,
                cargo_metadata::TargetKind::RLib { .. }
                    | cargo_metadata::TargetKind::DyLib { .. }
                    | cargo_metadata::TargetKind::CDyLib { .. }
                    | cargo_metadata::TargetKind::StaticLib { .. }
            )
        })
}

impl Check {
    pub fn new(current: Rustdoc) -> Self {
        Self {
            scope: Scope::default(),
            current,
            baseline: Rustdoc::from_registry_latest_crate_version(),
            release_type: None,
            current_feature_config: rustdoc_gen::FeatureConfig::default_for_current(),
            baseline_feature_config: rustdoc_gen::FeatureConfig::default_for_baseline(),
            build_target: None,
            witness_generation: WitnessGeneration::default(),
        }
    }

    pub fn set_package_selection(&mut self, selection: PackageSelection) -> &mut Self {
        self.scope.mode = ScopeMode::DenyList(selection);
        self
    }

    pub fn set_packages(&mut self, packages: Vec<String>) -> &mut Self {
        self.scope.mode = ScopeMode::AllowList(packages);
        self
    }

    pub fn set_baseline(&mut self, baseline: Rustdoc) -> &mut Self {
        self.baseline = baseline;
        self
    }

    pub fn set_release_type(&mut self, release_type: ReleaseType) -> &mut Self {
        self.release_type = Some(release_type);
        self
    }

    pub fn with_only_explicit_features(&mut self) -> &mut Self {
        self.current_feature_config.features_group = rustdoc_gen::FeaturesGroup::None;
        self.baseline_feature_config.features_group = rustdoc_gen::FeaturesGroup::None;
        self
    }

    pub fn with_default_features(&mut self) -> &mut Self {
        self.current_feature_config.features_group = rustdoc_gen::FeaturesGroup::Default;
        self.baseline_feature_config.features_group = rustdoc_gen::FeaturesGroup::Default;
        self
    }

    pub fn with_heuristically_included_features(&mut self) -> &mut Self {
        self.current_feature_config.features_group = rustdoc_gen::FeaturesGroup::Heuristic;
        self.baseline_feature_config.features_group = rustdoc_gen::FeaturesGroup::Heuristic;
        self
    }

    pub fn with_all_features(&mut self) -> &mut Self {
        self.current_feature_config.features_group = rustdoc_gen::FeaturesGroup::All;
        self.baseline_feature_config.features_group = rustdoc_gen::FeaturesGroup::All;
        self
    }

    pub fn set_extra_features(
        &mut self,
        extra_current_features: Vec<String>,
        extra_baseline_features: Vec<String>,
    ) -> &mut Self {
        self.current_feature_config.extra_features = extra_current_features;
        self.baseline_feature_config.extra_features = extra_baseline_features;
        self
    }

    /// Set what `--target` to build the documentation with, by default will not pass any flag
    /// relying on the users cargo configuration.
    pub fn set_build_target(&mut self, build_target: String) -> &mut Self {
        self.build_target = Some(build_target);
        self
    }

    /// Set the options for generating witness code.  See [`WitnessGeneration`] for more.
    pub fn set_witness_generation(&mut self, witness_generation: WitnessGeneration) -> &mut Self {
        self.witness_generation = witness_generation;
        self
    }

    /// Some `RustdocSource`s don't contain a path to the project root,
    /// so they don't have a target directory. We try to deduce the target directory
    /// on a "best effort" basis -- when the source contains a target dir,
    /// we use it, otherwise when the other source contains one, we use it,
    /// otherwise we just use a standard cache folder as specified by XDG.
    /// We cannot use a temporary directory, because the rustdocs from registry
    /// are being cached in the target directory.
    fn get_target_dir(&self, source: &RustdocSource) -> anyhow::Result<PathBuf> {
        Ok(
            if let Some(path) = get_target_dir_from_project_root(source)? {
                path
            } else if let Some(path) = get_target_dir_from_project_root(&self.current.source)? {
                path
            } else if let Some(path) = get_target_dir_from_project_root(&self.baseline.source)? {
                path
            } else {
                get_cache_dir()?
            },
        )
    }

    fn get_rustdoc_generator(
        &self,
        config: &mut GlobalConfig,
        source: &RustdocSource,
    ) -> anyhow::Result<rustdoc_gen::RustdocGenerator> {
        let target_dir = self.get_target_dir(source)?;
        Ok(match source {
            RustdocSource::Rustdoc(path) => {
                rustdoc_gen::RustdocFromFile::new(path.to_owned()).into()
            }
            RustdocSource::Root(root) => {
                rustdoc_gen::RustdocFromProjectRoot::new(root, &target_dir)?.into()
            }
            RustdocSource::Revision(root, rev) => {
                let metadata = manifest_metadata_no_deps(root)?;
                let source = metadata.workspace_root.as_std_path();
                rustdoc_gen::RustdocFromGitRevision::with_rev(source, &target_dir, rev, config)?
                    .into()
            }
            RustdocSource::VersionFromRegistry(version) => {
                let mut registry = rustdoc_gen::RustdocFromRegistry::new(&target_dir, config)?;
                if let Some(ver) = version {
                    let semver = semver::Version::parse(ver)?;
                    registry.set_version(semver);
                }
                registry.into()
            }
        })
    }

    pub fn check_release(&self, config: &mut GlobalConfig) -> anyhow::Result<Report> {
        let generation_settings = data_generation::GenerationSettings {
            use_color: config.err_color_choice(),
            deps: false,
            pass_through_stderr: config.is_verbose(),
        };

        // If both the current and baseline rustdoc are given explicitly as a file path,
        // we don't need to use the installed rustc, and this check can be skipped.
        if !(matches!(self.current.source, RustdocSource::Rustdoc(_))
            && matches!(self.baseline.source, RustdocSource::Rustdoc(_)))
        {
            let rustc_version_needed = config.minimum_rustc_version();
            match rustc_version::version() {
                Ok(rustc_version) => {
                    if rustc_version < *rustc_version_needed {
                        let help = "HELP: to use the latest rustc, run `rustup update stable && cargo +stable semver-checks <args>`";
                        anyhow::bail!(
                            "rustc version is not high enough: >={rustc_version_needed} needed, got {rustc_version}\n\n{help}"
                        );
                    }
                }
                Err(error) => {
                    let help = format!(
                        "HELP: to avoid errors please ensure rustc >={rustc_version_needed} is used"
                    );
                    config.shell_warn(format_args!(
                        "failed to determine the current rustc version: {error}\n\n{help}"
                    ))?;
                }
            };
        }

        let crates_to_check: Vec<CrateToCheck<'_>> = match &self.current.source {
            RustdocSource::Rustdoc(_)
            | RustdocSource::Revision(_, _)
            | RustdocSource::VersionFromRegistry(_) => {
                let names = match &self.scope.mode {
                    ScopeMode::DenyList(_) => match &self.current.source {
                        RustdocSource::Rustdoc(_) => {
                            // This is a user-facing string.
                            // For example, it appears when two pre-generated rustdoc files
                            // are semver-checked against each other.
                            vec!["<unknown>".to_string()]
                        }
                        _ => anyhow::bail!(
                            "couldn't deduce crate name, specify one through the package allow list"
                        ),
                    },
                    ScopeMode::AllowList(lst) => lst.clone(),
                };
                names
                    .into_iter()
                    .map(|name| {
                        let version = None;
                        CrateToCheck {
                            overrides: OverrideStack::new(),
                            current_crate_data: CrateDataForRustdoc {
                                crate_type: rustdoc_gen::CrateType::Current,
                                name: name.clone(),
                                feature_config: &self.current_feature_config,
                                build_target: self.build_target.as_deref(),
                            },
                            baseline_crate_data: CrateDataForRustdoc {
                                crate_type: rustdoc_gen::CrateType::Baseline {
                                    highest_allowed_version: version,
                                },
                                name,
                                feature_config: &self.baseline_feature_config,
                                build_target: self.build_target.as_deref(),
                            },
                        }
                    })
                    .collect()
            }
            RustdocSource::Root(project_root) => {
                let metadata = manifest_metadata(project_root)?;
                let (selected, skipped) = self.scope.selected_packages(&metadata);
                if selected.is_empty() {
                    let help = if skipped.is_empty() {
                        "".to_string()
                    } else {
                        let skipped = skipped.iter().map(|&p| &p.name).join(", ");
                        format!(
                            "
note: only library targets contain an API surface that can be checked for semver
note: skipped the following crates since they have no library target: {skipped}"
                        )
                    };
                    anyhow::bail!(
                        "no crates with library targets selected, nothing to semver-check{help}"
                    );
                }

                let workspace_overrides =
                    manifest::deserialize_lint_table(&metadata.workspace_metadata)
                        .context("[workspace.metadata.cargo-semver-checks] table is invalid")?
                        .map(|table| table.into_stack());

                selected
                    .iter()
                    .map(|selected| {
                        let crate_name = &selected.name;
                        let version = &selected.version;

                        // If the manifest we're using points to a workspace, then
                        // ignore `publish = false` crates unless they are specifically selected.
                        // If the manifest points to a specific crate, then check the crate
                        // even if `publish = false` is set.
                        let is_implied = matches!(self.scope.mode, ScopeMode::DenyList(..))
                            && metadata.workspace_members.len() > 1
                            && selected.publish == Some(vec![]);
                        if is_implied {
                            config.log_verbose(|config| {
                                config.shell_status(
                                    "Skipping",
                                    format_args!("{crate_name} v{version} (current)"),
                                )
                            })?;
                            Ok(None)
                        } else {
                            let overrides = overrides_for_workspace_package(
                                selected,
                                workspace_overrides.as_deref(),
                            )?;

                            Ok(Some(CrateToCheck {
                                overrides,
                                current_crate_data: CrateDataForRustdoc {
                                    crate_type: rustdoc_gen::CrateType::Current,
                                    name: crate_name.to_string(),
                                    feature_config: &self.current_feature_config,
                                    build_target: self.build_target.as_deref(),
                                },
                                baseline_crate_data: CrateDataForRustdoc {
                                    crate_type: rustdoc_gen::CrateType::Baseline {
                                        highest_allowed_version: Some(version.clone()),
                                    },
                                    name: crate_name.to_string(),
                                    feature_config: &self.baseline_feature_config,
                                    build_target: self.build_target.as_deref(),
                                },
                            }))
                        }
                    })
                    .filter_map(|res| res.transpose())
                    .collect::<Result<Vec<_>, anyhow::Error>>()?
            }
        };

        let current_loader = self.get_rustdoc_generator(config, &self.current.source)?;
        let baseline_loader = self.get_rustdoc_generator(config, &self.baseline.source)?;

        // Create a report for each crate.
        // We want to run all the checks, even if one returns `Err`.
        let all_outcomes: Vec<anyhow::Result<(String, CrateReport)>> = crates_to_check
            .into_iter()
            .map(|selected| {
                let start = std::time::Instant::now();
                let name = selected.current_crate_data.name.clone();

                let current_loader = rustdoc_gen::StatefulRustdocGenerator::couple_data(
                    &current_loader,
                    config,
                    &selected.current_crate_data,
                )
                .map_err(|err| log_terminal_error(config, err))?;
                let baseline_loader = rustdoc_gen::StatefulRustdocGenerator::couple_data(
                    &baseline_loader,
                    config,
                    &selected.baseline_crate_data,
                )
                .map_err(|err| log_terminal_error(config, err))?;

                let current_loader = current_loader
                    .prepare_generator(config)
                    .map_err(|err| log_terminal_error(config, err))?;
                let baseline_loader = baseline_loader
                    .prepare_generator(config)
                    .map_err(|err| log_terminal_error(config, err))?;

                let data_storage = generate_crate_data(
                    config,
                    generation_settings,
                    &current_loader,
                    &baseline_loader,
                )
                .map_err(|err| log_terminal_error(config, err))?;

                let report = run_check_release(
                    config,
                    &data_storage,
                    &name,
                    self.release_type,
                    &selected.overrides,
                    &self.witness_generation,
                )?;
                config.shell_status(
                    "Finished",
                    format_args!("[{:>8.3}s] {name}", start.elapsed().as_secs_f32()),
                )?;
                Ok((name, report))
            })
            .collect();
        let crate_reports: BTreeMap<String, CrateReport> = {
            let mut reports = BTreeMap::new();
            for outcome in all_outcomes {
                let (name, outcome) = outcome?;
                reports.insert(name, outcome);
            }
            reports
        };

        Ok(Report { crate_reports })
    }
}

fn overrides_for_workspace_package(
    package: &cargo_metadata::Package,
    workspace_overrides: Option<&[BTreeMap<String, QueryOverride>]>,
) -> Result<OverrideStack, anyhow::Error> {
    let lint_table = manifest::deserialize_lint_table(&package.metadata).with_context(|| {
        format!(
            "package `{}`'s [package.metadata.cargo-semver-checks] table is invalid (at {})",
            package.name, package.manifest_path,
        )
    })?;
    let selected_manifest =
        manifest::Manifest::parse_standalone(package.manifest_path.clone().into_std_path_buf())?;

    // N.B.: Do not use `==` here, because `==` is false for inherited values.
    let use_workspace_lints = matches!(
        selected_manifest.parsed.lints,
        cargo_toml::Inheritable::Inherited
    );
    let metadata_workspace_key = lint_table.as_ref().is_some_and(|x| x.workspace);

    let mut overrides = OverrideStack::new();
    if use_workspace_lints || metadata_workspace_key {
        if let Some(workspace) = workspace_overrides {
            for level in workspace {
                overrides.push(level);
            }
        }
    }
    if let Some(lint_table) = lint_table {
        for level in lint_table.into_stack() {
            overrides.push(&level);
        }
    }
    Ok(overrides)
}

#[cold]
fn log_terminal_error(config: &mut GlobalConfig, err: TerminalError) -> anyhow::Error {
    match err {
        TerminalError::WithAdvice(err, advice) => {
            if let Err(err) = config.log_error(|config| {
                writeln!(config.stderr(), "{advice}")?;
                Ok(())
            }) {
                return err;
            }
            err
        }
        TerminalError::Other(err) => err,
    }
}

/// Report of semver check of one crate.
#[non_exhaustive]
#[derive(Debug)]
pub struct CrateReport {
    /// Bump between the current version and the baseline one.
    detected_bump: ActualSemverUpdate,
    /// Minimum additional bump (on top of `detected_bump`) required to respect semver.
    /// For example, if the crate contains breaking changes, this is [`Some(ReleaseType::Major)`].
    /// If no additional bump beyond the already-detected one is required, this is [`Option::None`].
    required_bump: Option<ReleaseType>,
}

impl CrateReport {
    /// Check if the semver check was successful.
    /// `true` if required bump <= detected bump.
    pub fn success(&self) -> bool {
        match self.required_bump {
            // If `None`, no additional bump is required.
            None => true,
            // If `Some`, additional bump is required, so the report is not successful.
            Some(required_bump) => {
                // By design, `required_bump` should always be > `detected_bump`.
                // Let's assert that.
                match self.detected_bump {
                    // If user bumped the major version, any breaking change is accepted.
                    // So `required_bump` should be `None`.
                    ActualSemverUpdate::Major => {
                        panic!("detected_bump is major, while required_bump is {required_bump:?}")
                    }
                    ActualSemverUpdate::Minor => {
                        assert_eq!(required_bump, ReleaseType::Major);
                    }
                    ActualSemverUpdate::Patch | ActualSemverUpdate::NotChanged => {
                        assert!(matches!(
                            required_bump,
                            ReleaseType::Major | ReleaseType::Minor
                        ));
                    }
                }
                false
            }
        }
    }

    /// Minimum bump required to respect semver.
    /// It's [`Option::None`] if no bump is required beyond the already-detected bump.
    pub fn required_bump(&self) -> Option<ReleaseType> {
        self.required_bump
    }

    /// Bump between the current version and the baseline one.
    pub fn detected_bump(&self) -> ActualSemverUpdate {
        self.detected_bump
    }
}

/// Report of the whole analysis.
/// Contains a report for each crate checked.
#[non_exhaustive]
#[derive(Debug)]
pub struct Report {
    /// Collection containing the name and the report of each crate checked.
    crate_reports: BTreeMap<String, CrateReport>,
}

impl Report {
    /// `true` if none of the crates violate semver.
    pub fn success(&self) -> bool {
        self.crate_reports.values().all(|report| report.success())
    }

    /// Reports of each crate checked, sorted by crate name.
    pub fn crate_reports(&self) -> &BTreeMap<String, CrateReport> {
        &self.crate_reports
    }
}

/// Options for generating **witness code**.  A witness is a minimal buildable
/// example of how downstream code could break for a specific breaking change.
///
/// See also: [`Witness`]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct WitnessGeneration {
    /// Whether to print witness hints, short examples that show why a change is breaking,
    /// while not necessarily buildable standalone programs.  See [`Witness::hint_template`].
    pub show_hints: bool,
    /// Optional directory to write full witness examples to.  If this is `None`, full witnesses
    /// will not be generated.  See [`Witness::witness_template`].
    pub witness_directory: Option<PathBuf>,
}

impl WitnessGeneration {
    /// Creates a new [`WitnessGeneration`] instance indicating to not generate any witnesses.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            show_hints: false,
            witness_directory: None,
        }
    }
}

fn generate_crate_data(
    config: &mut GlobalConfig,
    generation_settings: data_generation::GenerationSettings,
    current_loader: &rustdoc_gen::StatefulRustdocGenerator<'_, rustdoc_gen::ReadyState<'_>>,
    baseline_loader: &rustdoc_gen::StatefulRustdocGenerator<'_, rustdoc_gen::ReadyState<'_>>,
) -> Result<DataStorage, TerminalError> {
    let current_crate = current_loader.load_rustdoc(
        config,
        generation_settings,
        data_generation::CacheSettings::ReadWrite(()),
    )?;

    let baseline_crate_name = &baseline_loader.get_crate_data().name;
    let current_rustdoc_version = current_crate.version();

    let baseline_crate = {
        let mut baseline_crate = baseline_loader.load_rustdoc(
            config,
            generation_settings,
            data_generation::CacheSettings::ReadWrite(()),
        )?;

        // The baseline rustdoc JSON may have been cached; ensure its rustdoc version matches
        // the version emitted by the currently-installed toolchain.
        //
        // The baseline and current rustdoc JSONs should have the same version.
        // If the baseline rustdoc version doesn't match, delete the cached baseline and rebuild it.
        //
        // Fix for: https://github.com/obi1kenobi/cargo-semver-checks/issues/415
        if baseline_crate.version() != current_rustdoc_version {
            let crate_name = baseline_crate_name;
            config
                .shell_status(
                    "Removing",
                    format_args!("stale cached baseline rustdoc for {crate_name}"),
                )
                .into_terminal_result()?;

            baseline_crate = baseline_loader.load_rustdoc(
                config,
                generation_settings,
                data_generation::CacheSettings::WriteOnly(()),
            )?;

            assert_eq!(
                baseline_crate.version(),
                current_rustdoc_version,
                "Deleting and regenerating the baseline JSON file did not resolve the rustdoc \
                 version mismatch."
            );
        }

        baseline_crate
    };

    // TODO: Temporary hack, until we stop supporting formats older than rustdoc v45.
    // v45+ formats carry the target triple information in the rustdoc JSON itself.
    let target_triple: &'static str = current_loader
        .get_crate_data()
        .build_target
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            let outcome = std::process::Command::new("rustc")
                .arg("-vV")
                .output()
                .expect("failed to run `rustc -vV`");
            let stdout = String::from_utf8(outcome.stdout).expect("stdout was not valid utf-8");
            let target_triple = stdout
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .expect("failed to find host line");
            target_triple.to_string()
        })
        .leak();
    Ok(DataStorage::new(
        current_crate,
        baseline_crate,
        target_triple,
    ))
}

fn manifest_path(project_root: &Path) -> anyhow::Result<PathBuf> {
    if project_root.is_dir() {
        let manifest_path = project_root.join("Cargo.toml");
        // Checking whether the file exists here is not necessary
        // (it will nevertheless be checked while parsing the manifest),
        // but it should give a nicer error message for the user.
        if manifest_path.exists() {
            Ok(manifest_path)
        } else {
            anyhow::bail!(
                "couldn't find Cargo.toml in directory {}",
                project_root.display()
            )
        }
    } else if project_root.ends_with("Cargo.toml") {
        // Even though the `project_root` should be a directory,
        // someone could by accident directly pass the path to the manifest
        // and we're kind enough to accept it.
        Ok(project_root.to_path_buf())
    } else {
        anyhow::bail!(
            "path {} is not a directory or a manifest",
            project_root.display()
        )
    }
}

fn manifest_metadata(project_root: &Path) -> anyhow::Result<cargo_metadata::Metadata> {
    let manifest_path = manifest_path(project_root)?;
    let mut command = cargo_metadata::MetadataCommand::new();
    let metadata = command.manifest_path(manifest_path).exec()?;
    Ok(metadata)
}

fn manifest_metadata_no_deps(project_root: &Path) -> anyhow::Result<cargo_metadata::Metadata> {
    let manifest_path = manifest_path(project_root)?;
    let mut command = cargo_metadata::MetadataCommand::new();
    let metadata = command.manifest_path(manifest_path).no_deps().exec()?;
    Ok(metadata)
}

fn get_cache_dir() -> anyhow::Result<PathBuf> {
    let project_dirs =
        ProjectDirs::from("", "", "cargo-semver-checks").context("can't determine project dirs")?;
    let cache_dir = project_dirs.cache_dir();
    std::fs::create_dir_all(cache_dir).context("can't create cache dir")?;
    Ok(cache_dir.to_path_buf())
}

fn get_target_dir_from_project_root(source: &RustdocSource) -> anyhow::Result<Option<PathBuf>> {
    Ok(match source {
        RustdocSource::Root(root) => {
            let metadata = manifest_metadata_no_deps(root)?;
            let target = metadata.target_directory.as_std_path().join(util::SCOPE);
            Some(target)
        }
        RustdocSource::Revision(root, rev) => {
            let metadata = manifest_metadata_no_deps(root)?;
            let target = metadata.target_directory.as_std_path().join(util::SCOPE);
            let target = target.join(format!("git-{}", util::slugify(rev)));
            Some(target)
        }
        RustdocSource::Rustdoc(_path) => None,
        RustdocSource::VersionFromRegistry(_version) => None,
    })
}
