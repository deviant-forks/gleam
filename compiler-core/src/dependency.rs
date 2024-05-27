use std::{borrow::Borrow, cell::RefCell, collections::HashMap, error::Error as StdError};

use crate::{Error, Result};

use ecow::EcoString;
use hexpm::{
    version::{Range, Version},
    Dependency, Release,
};
use pubgrub::{
    error::PubGrubError,
    solver::{choose_package_with_fewest_versions, Dependencies},
    type_aliases::Map,
};

pub type PackageVersions = HashMap<String, Version>;

pub type ResolutionError = PubGrubError<String, Version>;

type PubgrubRange = pubgrub::range::Range<Version>;

pub fn resolve_versions<Requirements>(
    package_fetcher: Box<dyn PackageFetcher>,
    provided_packages: HashMap<EcoString, hexpm::Package>,
    root_name: EcoString,
    dependencies: Requirements,
    locked: &HashMap<EcoString, Version>,
) -> Result<PackageVersions>
where
    Requirements: Iterator<Item = (EcoString, Range)>,
{
    tracing::info!("resolving_versions");
    let root_version = Version::new(0, 0, 0);
    let requirements =
        root_dependencies(dependencies, locked).map_err(Error::dependency_resolution_failed)?;

    // Creating a map of all the required packages that have exact versions specified
    let exact_deps = &requirements
        .iter()
        .filter_map(|(name, dep)| parse_exact_version(dep.requirement.as_str()).map(|v| (name, v)))
        .map(|(name, version)| (name.clone(), version))
        .collect();

    let root = hexpm::Package {
        name: root_name.as_str().into(),
        repository: "local".into(),
        releases: vec![Release {
            version: root_version.clone(),
            outer_checksum: vec![],
            retirement_status: None,
            requirements,
            meta: (),
        }],
    };

    dbg!(&root, &exact_deps);

    let dependency_provider =
        DependencyProvider::new(package_fetcher, provided_packages, root, locked, exact_deps);
    let dependency_provider = DependencyProviderProxy {
        provider: dependency_provider,
    };

    let packages = pubgrub::solver::resolve(
        &dependency_provider,
        root_name.as_str().into(),
        root_version,
    )
    .map_err(Error::dependency_resolution_failed)?
    .into_iter()
    .filter(|(name, _)| name.as_str() != root_name.as_str())
    .collect();

    Ok(packages)
}

// If the string would parse to an exact version then return the version
fn parse_exact_version(ver: &str) -> Option<Version> {
    let version = ver.trim();
    let first_byte = version.as_bytes().first();

    // Version is exact if it starts with an explicit == or a number
    if version.starts_with("==") || first_byte.map_or(false, |v| v.is_ascii_digit()) {
        let version = version.replace("==", "");
        let version = version.as_str().trim();
        if let Ok(v) = Version::parse(version) {
            Some(v)
        } else {
            None
        }
    } else {
        None
    }
}

fn root_dependencies<Requirements>(
    base_requirements: Requirements,
    locked: &HashMap<EcoString, Version>,
) -> Result<HashMap<String, Dependency>, ResolutionError>
where
    Requirements: Iterator<Item = (EcoString, Range)>,
{
    // Record all of the already locked versions as hard requirements
    let mut requirements: HashMap<_, _> = locked
        .iter()
        .map(|(name, version)| {
            (
                name.to_string(),
                Dependency {
                    app: None,
                    optional: false,
                    repository: None,
                    requirement: Range::new(version.to_string()),
                },
            )
        })
        .collect();

    for (name, range) in base_requirements {
        match locked.get(&name) {
            // If the package was not already locked then we can use the
            // specified version requirement without modification.
            None => {
                let _ = requirements.insert(
                    name.into(),
                    Dependency {
                        app: None,
                        optional: false,
                        repository: None,
                        requirement: range,
                    },
                );
            }

            // If the version was locked we verify that the requirement is
            // compatible with the locked version.
            Some(locked_version) => {
                let compatible = range
                    .to_pubgrub()
                    .map_err(|e| ResolutionError::Failure(format!("Failed to parse range {}", e)))?
                    .contains(locked_version);
                if !compatible {
                    return Err(ResolutionError::Failure(format!(
                        "{package} is specified with the requirement `{requirement}`, \
but it is locked to {version}, which is incompatible.",
                        package = name,
                        requirement = range,
                        version = locked_version,
                    )));
                }
            }
        };
    }

    Ok(requirements)
}

pub trait PackageFetcher {
    fn get_dependencies(&self, package: &str) -> Result<hexpm::Package, Box<dyn StdError>>;
}

struct DependencyProvider<'a> {
    packages: RefCell<HashMap<EcoString, hexpm::Package>>,
    remote: Box<dyn PackageFetcher>,
    locked: &'a HashMap<EcoString, Version>,
    // Map of packages where an exact version was requested
    // We need this because by default pubgrub checks exact version by checking if a version is between the exact
    // and the version 1 bump ahead. That default breaks on prerelease builds since a bump includes the whole patch
    exact_only: &'a HashMap<String, Version>,
}

impl<'a> DependencyProvider<'a> {
    fn new(
        remote: Box<dyn PackageFetcher>,
        mut packages: HashMap<EcoString, hexpm::Package>,
        root: hexpm::Package,
        locked: &'a HashMap<EcoString, Version>,
        exact_only: &'a HashMap<String, Version>,
    ) -> Self {
        let _ = packages.insert(root.name.as_str().into(), root);
        Self {
            packages: RefCell::new(packages),
            locked,
            remote,
            exact_only,
        }
    }

    /// Download information about the package from the registry into the local
    /// store. Does nothing if the packages are already known.
    ///
    /// Package versions are sorted from newest to oldest, with all pre-releases
    /// at the end to ensure that a non-prerelease version will be picked first
    /// if there is one.
    //
    fn ensure_package_fetched(
        // We would like to use `&mut self` but the pubgrub library enforces
        // `&self` with interop mutability.
        &self,
        name: &str,
    ) -> Result<(), Box<dyn StdError>> {
        let mut packages = self.packages.borrow_mut();
        if packages.get(name).is_none() {
            let mut package = self.remote.get_dependencies(name)?;
            // Sort the packages from newest to oldest, pres after all others
            package.releases.sort_by(|a, b| a.version.cmp(&b.version));
            package.releases.reverse();
            let (pre, mut norm): (_, Vec<_>) = package
                .releases
                .into_iter()
                .partition(|r| r.version.is_pre());
            norm.extend(pre);
            package.releases = norm;
            let _ = packages.insert(name.into(), package);
        }
        Ok(())
    }
}

type PackageName = String;

impl<'a> pubgrub::solver::DependencyProvider<PackageName, Version> for DependencyProvider<'a> {
    fn choose_package_version<Name: Borrow<PackageName>, Ver: Borrow<PubgrubRange>>(
        &self,
        potential_packages: impl Iterator<Item = (Name, Ver)>,
    ) -> Result<(Name, Option<Version>), Box<dyn StdError>> {
        let potential_packages: Vec<_> = potential_packages
            .map::<Result<_, Box<dyn StdError>>, _>(|pair| {
                self.ensure_package_fetched(pair.0.borrow())?;
                Ok(pair)
            })
            .collect::<Result<_, _>>()?;
        let list_available_versions = |name: &String| {
            let name = name.as_str();
            let exact_package = self.exact_only.get(name);
            let versions = self
                .packages
                .borrow()
                .get(name)
                .cloned()
                .into_iter()
                .flat_map(move |p| {
                    p.releases
                        .into_iter()
                        // if an exact version of a package is specified then we only want to allow that version as available
                        .filter(move |release| match exact_package {
                            Some(ver) => ver == &release.version,
                            _ => true,
                        })
                })
                .map(|p| p.version)
                .collect::<Vec<_>>();

            // for version in versions.iter() {
            //     println!(
            //         "this.available_versions.entry(\"{name}\".to_string()).or_default().push(Version::parse(\"{version}\").unwrap());");
            // }

            // println!(
            //     "this.available_versions.entry(\"{name}\".to_string()).or_default() has the following versions: {:?}",
            //     versions
            //         .iter()
            //         .map(|version| version.to_string())
            //         .collect::<Vec<_>>()
            // );

            versions.into_iter()
        };
        Ok(choose_package_with_fewest_versions(
            list_available_versions,
            potential_packages.into_iter(),
        ))
    }

    fn get_dependencies(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Result<Dependencies<PackageName, Version>, Box<dyn StdError>> {
        self.ensure_package_fetched(name)?;
        let packages = self.packages.borrow();
        let release = match packages
            .get(name.as_str())
            .into_iter()
            .flat_map(|p| p.releases.iter())
            .find(|r| &r.version == version)
        {
            Some(release) => release,
            None => return Ok(Dependencies::Unknown),
        };

        // Only use retired versions if they have been locked
        if release.is_retired() && self.locked.get(name.as_str()) != Some(version) {
            return Ok(Dependencies::Unknown);
        }

        let mut deps: Map<String, PubgrubRange> = Default::default();
        for (name, d) in &release.requirements {
            let range = d.requirement.to_pubgrub()?;
            let _ = deps.insert(name.clone(), range);
        }
        Ok(Dependencies::Known(deps))
    }
}

struct DependencyProviderProxy<'a> {
    provider: DependencyProvider<'a>,
}

impl<'a> pubgrub::solver::DependencyProvider<PackageName, Version> for DependencyProviderProxy<'a> {
    fn choose_package_version<Name: Borrow<PackageName>, Ver: Borrow<PubgrubRange>>(
        &self,
        potential_packages: impl Iterator<Item = (Name, Ver)>,
    ) -> Result<(Name, Option<Version>), Box<dyn StdError>> {
        let result = self.provider.choose_package_version(potential_packages);
        // println!(
        //     "choose_package_version returned {:?}",
        //     result
        //         .as_ref()
        //         .map(|(name, version)| { (name.borrow().to_string(), version) })
        // );
        result
    }

    fn get_dependencies(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Result<Dependencies<PackageName, Version>, Box<dyn StdError>> {
        let result = self.provider.get_dependencies(name, version);

        if name.as_str() != "argv" {
            return result;
        }

        if let Ok(result) = result.as_ref() {
            match result {
                Dependencies::Unknown => {
                    println!("this.dependencies.entry((\"{name}\".to_string(), Version::parse(\"{version}\").unwrap())).or_default().push(Dependencies::Unknown);");
                }
                Dependencies::Known(deps) => {
                    let deps_literal = format!(
                        "Map::from_iter([{}])",
                        deps.into_iter()
                            .map(|(name, version_range)| {
                                // dbg!(&version_range);
                                format!(
                                    "(\"{name}\".to_string(), Range::new(\"{}\".to_string()).to_pubgrub().unwrap())",
                                    version_range.to_string()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",\n")
                    );

                    println!("let _ = this.dependencies.insert((\"{name}\".to_string(), Version::parse(\"{version}\").unwrap()), Dependencies::Known({deps_literal}));");
                    // for (name, version_range) in deps {
                    // }
                }
            }
        }

        // println!(
        //     "get_dependencies({name}, {version}) returned {:?}",
        //     result.as_ref().map(|deps| match deps {
        //         Dependencies::Unknown => "Unknown".to_string(),
        //         Dependencies::Known(deps) => format!("{deps:?}"),
        //     })
        // );
        result
    }
}

struct Issue3201DependencyProvider {
    available_versions: HashMap<PackageName, Vec<Version>>,
    dependencies: HashMap<(PackageName, Version), Dependencies<PackageName, Version>>,
}

impl pubgrub::solver::DependencyProvider<PackageName, Version> for Issue3201DependencyProvider {
    fn choose_package_version<Name: Borrow<PackageName>, Ver: Borrow<PubgrubRange>>(
        &self,
        potential_packages: impl Iterator<Item = (Name, Ver)>,
    ) -> Result<(Name, Option<Version>), Box<dyn StdError>> {
        Ok(choose_package_with_fewest_versions(
            |name: &String| {
                let Some(available_versions) = self.available_versions.get(name) else {
                    return Vec::new().into_iter();
                };

                available_versions.clone().into_iter()
            },
            potential_packages.into_iter(),
        ))
    }

    fn get_dependencies(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Result<Dependencies<PackageName, Version>, Box<dyn StdError>> {
        self.dependencies
            .get(&(name.clone(), version.clone()))
            .cloned()
            .ok_or_else(|| "failed to get dependencies".into())
    }
}

impl Issue3201DependencyProvider {
    pub fn new() -> Self {
        let mut this = Self {
            available_versions: HashMap::default(),
            dependencies: HashMap::default(),
        };

        this.available_versions
            .entry("gleam_add_issue_2024_05_26".to_string())
            .or_default()
            .push(Version::parse("0.0.0").unwrap());
        this.available_versions
            .entry("argv".to_string())
            .or_default()
            .push(Version::parse("1.0.2").unwrap());
        this.available_versions
            .entry("birl".to_string())
            .or_default()
            .push(Version::parse("1.7.0").unwrap());
        this.available_versions
            .entry("gleam_javascript".to_string())
            .or_default()
            .push(Version::parse("0.8.0").unwrap());
        this.available_versions
            .entry("gleam_community_colour".to_string())
            .or_default()
            .push(Version::parse("1.4.0").unwrap());
        this.available_versions
            .entry("gleam_community_ansi".to_string())
            .or_default()
            .push(Version::parse("1.4.0").unwrap());
        this.available_versions
            .entry("gleam_erlang".to_string())
            .or_default()
            .push(Version::parse("0.25.0").unwrap());
        this.available_versions
            .entry("tom".to_string())
            .or_default()
            .push(Version::parse("0.3.0").unwrap());
        this.available_versions
            .entry("thoas".to_string())
            .or_default()
            .push(Version::parse("1.2.1").unwrap());
        this.available_versions
            .entry("glint".to_string())
            .or_default()
            .push(Version::parse("1.0.0-rc2").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.14.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.13.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.12.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.11.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.10.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.9.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.8.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.7.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.6.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.5.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.4.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.3.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.2.0").unwrap());
        this.available_versions
            .entry("wisp".to_string())
            .or_default()
            .push(Version::parse("0.1.0").unwrap());
        this.available_versions
            .entry("snag".to_string())
            .or_default()
            .push(Version::parse("0.3.0").unwrap());
        this.available_versions
            .entry("gleam_otp".to_string())
            .or_default()
            .push(Version::parse("0.10.0").unwrap());
        this.available_versions
            .entry("exception".to_string())
            .or_default()
            .push(Version::parse("2.0.0").unwrap());
        this.available_versions
            .entry("ranger".to_string())
            .or_default()
            .push(Version::parse("1.2.0").unwrap());
        this.available_versions
            .entry("simplifile".to_string())
            .or_default()
            .push(Version::parse("1.7.0").unwrap());
        this.available_versions
            .entry("filepath".to_string())
            .or_default()
            .push(Version::parse("1.0.0").unwrap());
        this.available_versions
            .entry("startest".to_string())
            .or_default()
            .push(Version::parse("0.2.4").unwrap());
        this.available_versions
            .entry("gleam_json".to_string())
            .or_default()
            .push(Version::parse("1.0.1").unwrap());
        this.available_versions
            .entry("bigben".to_string())
            .or_default()
            .push(Version::parse("1.0.0").unwrap());
        this.available_versions
            .entry("gleam_stdlib".to_string())
            .or_default()
            .push(Version::parse("0.38.0").unwrap());

        let _ = this.dependencies.insert(
            (
                "gleam_add_issue_2024_05_26".to_string(),
                Version::parse("0.0.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([
                (
                    "bigben".to_string(),
                    Range::new("1.0.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new("0.38.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_javascript".to_string(),
                    Range::new("0.8.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_community_colour".to_string(),
                    Range::new("1.4.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_community_ansi".to_string(),
                    Range::new("1.4.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_erlang".to_string(),
                    Range::new("0.25.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "tom".to_string(),
                    Range::new("0.3.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "thoas".to_string(),
                    Range::new("1.2.1".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "glint".to_string(),
                    Range::new("1.0.0-rc2".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "wisp".to_string(),
                    pubgrub::range::Range::any(),
                    // Range::new("*".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "snag".to_string(),
                    Range::new("0.3.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_otp".to_string(),
                    Range::new("0.10.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "simplifile".to_string(),
                    Range::new("1.7.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "ranger".to_string(),
                    Range::new("1.2.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "exception".to_string(),
                    Range::new("2.0.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "filepath".to_string(),
                    Range::new("1.0.0".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "gleam_json".to_string(),
                    Range::new("1.0.1".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "startest".to_string(),
                    Range::new("0.2.4".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "argv".to_string(),
                    Range::new("1.0.2".to_string()).to_pubgrub().unwrap(),
                ),
                (
                    "birl".to_string(),
                    Range::new("1.7.0".to_string()).to_pubgrub().unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            (
                "gleam_stdlib".to_string(),
                Version::parse("0.38.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([])),
        );
        let _ = this.dependencies.insert(
            ("argv".to_string(), Version::parse("1.0.2").unwrap()),
            Dependencies::Known(Map::from_iter([])),
        );
        let _ = this.dependencies.insert(
            ("birl".to_string(), Version::parse("1.7.0").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.37.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "ranger".to_string(),
                    Range::new(">= 1.2.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            ("bigben".to_string(), Version::parse("1.0.0").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_otp".to_string(),
                    Range::new(">= 0.10.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.34.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_erlang".to_string(),
                    Range::new(">= 0.25.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "birl".to_string(),
                    Range::new(">= 1.6.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            (
                "gleam_javascript".to_string(),
                Version::parse("0.8.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.19.0 and < 2.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            (
                "gleam_community_colour".to_string(),
                Version::parse("1.4.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.34.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_json".to_string(),
                    Range::new(">= 0.7.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            (
                "gleam_community_ansi".to_string(),
                Version::parse("1.4.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_community_colour".to_string(),
                    Range::new(">= 1.3.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.34.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            (
                "gleam_erlang".to_string(),
                Version::parse("0.25.0").unwrap(),
            ),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.33.0 and < 2.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("tom".to_string(), Version::parse("0.3.0").unwrap()),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.33.0 and < 1.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("thoas".to_string(), Version::parse("1.2.1").unwrap()),
            Dependencies::Known(Map::from_iter([])),
        );
        let _ = this.dependencies.insert(
            ("glint".to_string(), Version::parse("1.0.0-rc2").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_community_colour".to_string(),
                    Range::new(">= 1.0.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_community_ansi".to_string(),
                    Range::new(">= 1.0.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "snag".to_string(),
                    Range::new(">= 0.3.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.36.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            ("snag".to_string(), Version::parse("0.3.0").unwrap()),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.34.0 and < 1.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("gleam_otp".to_string(), Version::parse("0.10.0").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "gleam_erlang".to_string(),
                    Range::new(">= 0.22.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.32.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            ("exception".to_string(), Version::parse("2.0.0").unwrap()),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.30.0 and < 2.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("ranger".to_string(), Version::parse("1.2.0").unwrap()),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.36.0 and < 2.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("simplifile".to_string(), Version::parse("1.7.0").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "filepath".to_string(),
                    Range::new(">= 1.0.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.34.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );
        let _ = this.dependencies.insert(
            ("filepath".to_string(), Version::parse("1.0.0").unwrap()),
            Dependencies::Known(Map::from_iter([(
                "gleam_stdlib".to_string(),
                Range::new(">= 0.32.0 and < 1.0.0".to_string())
                    .to_pubgrub()
                    .unwrap(),
            )])),
        );
        let _ = this.dependencies.insert(
            ("startest".to_string(), Version::parse("0.2.4").unwrap()),
            Dependencies::Known(Map::from_iter([
                (
                    "argv".to_string(),
                    Range::new(">= 1.0.2 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_stdlib".to_string(),
                    Range::new(">= 0.36.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "exception".to_string(),
                    Range::new(">= 2.0.0 and < 3.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "simplifile".to_string(),
                    Range::new(">= 1.7.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_javascript".to_string(),
                    Range::new(">= 0.8.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_community_ansi".to_string(),
                    Range::new(">= 1.4.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "gleam_erlang".to_string(),
                    Range::new(">= 0.25.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "tom".to_string(),
                    Range::new(">= 0.3.0 and < 1.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "glint".to_string(),
                    Range::new(">= 1.0.0-rc2 and < 1.0.0-rc3".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "bigben".to_string(),
                    Range::new(">= 1.0.0 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
                (
                    "birl".to_string(),
                    Range::new(">= 1.6.1 and < 2.0.0".to_string())
                        .to_pubgrub()
                        .unwrap(),
                ),
            ])),
        );

        this
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Remote {
        deps: HashMap<String, hexpm::Package>,
    }

    impl PackageFetcher for Remote {
        fn get_dependencies(&self, package: &str) -> Result<hexpm::Package, Box<dyn StdError>> {
            self.deps
                .get(package)
                .cloned()
                .ok_or(Box::new(hexpm::ApiError::NotFound))
        }
    }

    fn make_remote() -> Box<Remote> {
        let mut deps = HashMap::new();
        let _ = deps.insert(
            "gleam_stdlib".into(),
            hexpm::Package {
                name: "gleam_stdlib".into(),
                repository: "hexpm".into(),
                releases: vec![
                    Release {
                        version: Version::try_from("0.1.0").unwrap(),
                        requirements: [].into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.2.0").unwrap(),
                        requirements: [].into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.2.2").unwrap(),
                        requirements: [].into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.3.0").unwrap(),
                        requirements: [].into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                ],
            },
        );
        let _ = deps.insert(
            "gleam_otp".into(),
            hexpm::Package {
                name: "gleam_otp".into(),
                repository: "hexpm".into(),
                releases: vec![
                    Release {
                        version: Version::try_from("0.1.0").unwrap(),
                        requirements: [(
                            "gleam_stdlib".into(),
                            Dependency {
                                app: None,
                                optional: false,
                                repository: None,
                                requirement: Range::new(">= 0.1.0".into()),
                            },
                        )]
                        .into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.2.0").unwrap(),
                        requirements: [(
                            "gleam_stdlib".into(),
                            Dependency {
                                app: None,
                                optional: false,
                                repository: None,
                                requirement: Range::new(">= 0.1.0".into()),
                            },
                        )]
                        .into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.3.0-rc1").unwrap(),
                        requirements: [(
                            "gleam_stdlib".into(),
                            Dependency {
                                app: None,
                                optional: false,
                                repository: None,
                                requirement: Range::new(">= 0.1.0".into()),
                            },
                        )]
                        .into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.3.0-rc2").unwrap(),
                        requirements: [(
                            "gleam_stdlib".into(),
                            Dependency {
                                app: None,
                                optional: false,
                                repository: None,
                                requirement: Range::new(">= 0.1.0".into()),
                            },
                        )]
                        .into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                ],
            },
        );
        let _ = deps.insert(
            "package_with_retired".into(),
            hexpm::Package {
                name: "package_with_retired".into(),
                repository: "hexpm".into(),
                releases: vec![
                    Release {
                        version: Version::try_from("0.1.0").unwrap(),
                        requirements: [].into(),
                        retirement_status: None,
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                    Release {
                        version: Version::try_from("0.2.0").unwrap(),
                        requirements: [].into(),
                        retirement_status: Some(hexpm::RetirementStatus {
                            reason: hexpm::RetirementReason::Security,
                            message: "It's bad".into(),
                        }),
                        outer_checksum: vec![1, 2, 3],
                        meta: (),
                    },
                ],
            },
        );
        Box::new(Remote { deps })
    }

    #[test]
    fn resolution_with_locked() {
        let locked_stdlib = ("gleam_stdlib".into(), Version::parse("0.1.0").unwrap());
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_stdlib".into(), Range::new("~> 0.1".into()))].into_iter(),
            &vec![locked_stdlib].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![("gleam_stdlib".into(), Version::parse("0.1.0").unwrap())]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn resolution_without_deps() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(result, vec![].into_iter().collect())
    }

    #[test]
    fn resolution_1_dep() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_stdlib".into(), Range::new("~> 0.1".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![("gleam_stdlib".into(), Version::try_from("0.3.0").unwrap())]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn resolution_with_nested_deps() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_otp".into(), Range::new("~> 0.1".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                ("gleam_otp".into(), Version::try_from("0.2.0").unwrap()),
                ("gleam_stdlib".into(), Version::try_from("0.3.0").unwrap())
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn resolution_locked_to_older_version() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_otp".into(), Range::new("~> 0.1.0".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                ("gleam_otp".into(), Version::try_from("0.1.0").unwrap()),
                ("gleam_stdlib".into(), Version::try_from("0.3.0").unwrap())
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn resolution_retired_versions_not_used_by_default() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("package_with_retired".into(), Range::new("> 0.0.0".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![(
                "package_with_retired".into(),
                // Uses the older version that hasn't been retired
                Version::try_from("0.1.0").unwrap()
            ),]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn resolution_retired_versions_can_be_used_if_locked() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("package_with_retired".into(), Range::new("> 0.0.0".into()))].into_iter(),
            &vec![("package_with_retired".into(), Version::new(0, 2, 0))]
                .into_iter()
                .collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![(
                "package_with_retired".into(),
                // Uses the locked version even though it's retired
                Version::new(0, 2, 0)
            ),]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn resolution_prerelease_can_be_selected() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_otp".into(), Range::new("~> 0.3.0-rc1".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                ("gleam_stdlib".into(), Version::try_from("0.3.0").unwrap()),
                ("gleam_otp".into(), Version::try_from("0.3.0-rc2").unwrap()),
            ]
            .into_iter()
            .collect(),
        );
    }

    #[test]
    fn resolution_exact_prerelease_can_be_selected() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_otp".into(), Range::new("0.3.0-rc1".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                ("gleam_stdlib".into(), Version::try_from("0.3.0").unwrap()),
                ("gleam_otp".into(), Version::try_from("0.3.0-rc1").unwrap()),
            ]
            .into_iter()
            .collect(),
        );
    }

    #[test]
    fn resolution_not_found_dep() {
        let _ = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("unknown".into(), Range::new("~> 0.1".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap_err();
    }

    #[test]
    fn resolution_no_matching_version() {
        let _ = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_stdlib".into(), Range::new("~> 99.0".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap_err();
    }

    #[test]
    fn resolution_locked_version_doesnt_satisfy_requirements() {
        let err = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_stdlib".into(), Range::new("~> 0.1.0".into()))].into_iter(),
            &vec![("gleam_stdlib".into(), Version::new(0, 2, 0))]
                .into_iter()
                .collect(),
        )
        .unwrap_err();

        match err {
        Error::DependencyResolutionFailed(msg) => assert_eq!(
            msg,
            "An unrecoverable error happened while solving dependencies: gleam_stdlib is specified with the requirement `~> 0.1.0`, but it is locked to 0.2.0, which is incompatible."
        ),
        _ => panic!("wrong error: {}", err),
        }
    }

    #[test]
    fn resolution_with_exact_dep() {
        let result = resolve_versions(
            make_remote(),
            HashMap::new(),
            "app".into(),
            vec![("gleam_stdlib".into(), Range::new("0.1.0".into()))].into_iter(),
            &vec![].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            result,
            vec![("gleam_stdlib".into(), Version::try_from("0.1.0").unwrap())]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn parse_exact_version_test() {
        assert_eq!(
            parse_exact_version("1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_exact_version("==1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_exact_version("== 1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(parse_exact_version("~> 1.0.0"), None);
        assert_eq!(parse_exact_version(">= 1.0.0"), None);
    }

    #[test]
    fn issue_3201_reproduction_test() {
        let dependency_provider = Issue3201DependencyProvider::new();

        let result = pubgrub::solver::resolve(
            &dependency_provider,
            "gleam_add_issue_2024_05_26".into(),
            Version::new(0, 0, 0),
        );

        dbg!(&result);

        assert!(result.is_ok());
    }
}
