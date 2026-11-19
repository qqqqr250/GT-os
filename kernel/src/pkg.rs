//! Clean-room package state management, inspired by Redox `pkgutils`.
//!
//! Redox does not track packages as a flat list of booleans: `pkg-lib` keeps a
//! dependency graph with manual vs. automatically installed packages, protected
//! packages that cannot be removed, and install/uninstall resolution. This
//! module adopts that model so `GTos`'s `pkg` command can grow beyond a static
//! `installed = true` flag.
//!
//! `GTos` has no remote repository, so "install" here only updates this in-memory
//! state against a catalog compiled into the kernel. The graph rules are the
//! same ones `pkg` uses on Redox, so a real backend can be added later without
//! changing the model.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

/// Guard against a malformed catalog with a dependency cycle. A visited set
/// already makes traversal terminate; this is a second, cheap safety net.
const MAX_DEPTH: usize = 100;

/// Errors from package name validation and dependency-graph operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageError {
    /// A name did not follow `[host:|target:]recipe[.suffix]`.
    InvalidName(String),
    /// No catalog entry has this name.
    NotFound(PackageName),
    /// This package is protected and cannot be uninstalled.
    Protected(PackageName),
    /// Other installed packages still depend on this one.
    RequiredBy {
        package: PackageName,
        dependents: Vec<PackageName>,
    },
    /// A dependency of `package` is absent from the catalog.
    MissingDependency {
        package: PackageName,
        dependency: PackageName,
    },
    /// The catalog is too deep to resolve safely.
    DependencyLimit(PackageName),
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid package name: {name}"),
            Self::NotFound(name) => write!(f, "unknown package: {name}"),
            Self::Protected(name) => write!(f, "package {name} is protected"),
            Self::RequiredBy {
                package,
                dependents,
            } => {
                let names: Vec<&str> = dependents.iter().map(PackageName::as_str).collect();
                write!(f, "{package} is required by {}", names.join(", "))
            }
            Self::MissingDependency {
                package,
                dependency,
            } => write!(f, "{package} needs missing dependency {dependency}"),
            Self::DependencyLimit(name) => write!(f, "dependency limit reached at {name}"),
        }
    }
}

/// A validated package name: `[host:|target:]recipe[.suffix]`.
///
/// Uses the same grammar as Redox: at most one `host:`/`target:` prefix and one
/// `.` suffix, with no `/` or NUL bytes. The prefix and suffix are kept as part
/// of the name so `host:foo` and `foo.pkg` remain distinct packages.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PackageName(String);

impl PackageName {
    /// Validates and wraps a package name.
    pub fn new(name: &str) -> Result<Self, PackageError> {
        let invalid = name.is_empty()
            || name.contains('/')
            || name.contains('\0')
            || name.matches('.').count() > 1
            || name.matches(':').count() > 1;
        if invalid {
            return Err(PackageError::InvalidName(name.to_string()));
        }
        // A colon is only allowed as part of a recognised prefix.
        if name.contains(':') && Self::split(name).0.is_empty() {
            return Err(PackageError::InvalidName(name.to_string()));
        }
        Ok(Self(String::from(name)))
    }

    /// Only for catalog entries whose names are compile-time constants.
    fn from_static(name: &'static str) -> Self {
        debug_assert!(Self::new(name).is_ok(), "invalid built-in package: {name}");
        Self(String::from(name))
    }

    /// Splits a name into `(prefix, recipe, suffix)`.
    fn split(name: &str) -> (&str, &str, Option<&str>) {
        let (prefix, rest) = if let Some(rest) = name.strip_prefix("host:") {
            ("host:", rest)
        } else if let Some(rest) = name.strip_prefix("target:") {
            ("target:", rest)
        } else {
            ("", name)
        };
        match rest.find('.') {
            Some(index) => (prefix, &rest[..index], Some(&rest[index + 1..])),
            None => (prefix, rest, None),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for a `host:`-prefixed name.
    #[must_use]
    pub fn is_host(&self) -> bool {
        self.0.starts_with("host:")
    }

    /// True for a `target:`-prefixed name.
    #[must_use]
    pub fn is_target(&self) -> bool {
        self.0.starts_with("target:")
    }

    /// The recipe between the prefix and the `.` suffix.
    #[must_use]
    pub fn recipe(&self) -> &str {
        Self::split(&self.0).1
    }

    /// The `.pkg`-style suffix, if any.
    #[must_use]
    pub fn suffix(&self) -> Option<&str> {
        Self::split(&self.0).2
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a recipe obtains its files, mirroring cookbook's `SourceRecipe`: a
/// component compiled into the kernel, a `.pkga` archive on the data disk, or a
/// meta package that owns no files at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceKind {
    /// Compiled into the kernel image; no external source is needed.
    Builtin,
    /// A `.pkga` archive on the FAT data disk (file name).
    Archive(&'static str),
    /// A meta/group package: no source, only dependency edges.
    Meta,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin => f.write_str("builtin"),
            Self::Archive(name) => write!(f, "archive:{name}"),
            Self::Meta => f.write_str("meta"),
        }
    }
}

/// How a recipe is built, mirroring cookbook's `BuildKind`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildKind {
    /// No build step (meta packages and prebuilt archives).
    None,
    /// Compiled into the kernel image.
    Builtin,
    /// Unpacked from a `.pkga` archive.
    Archive,
}

impl fmt::Display for BuildKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Builtin => "builtin",
            Self::Archive => "archive",
        })
    }
}

/// Cookbook-style metadata describing where a package comes from, how it is
/// built and what it needs. Cookbook's `[build]` and `[package]` sections keep
/// build-time dependencies separate from runtime ones; `GTos` keeps the same
/// distinction even though it has no in-guest compiler yet.
#[derive(Clone, Debug)]
pub struct Recipe {
    pub source: SourceKind,
    pub build: BuildKind,
    /// Needed only while building (cookbook's `[build] dependencies`).
    pub build_depends: Vec<PackageName>,
    /// Needed at runtime (cookbook's `[package] dependencies`).
    pub package_depends: Vec<PackageName>,
}

impl Recipe {
    fn new(
        source: SourceKind,
        build: BuildKind,
        build_depends: &[&'static str],
        package_depends: &[&'static str],
    ) -> Self {
        Self {
            source,
            build,
            build_depends: build_depends
                .iter()
                .map(|name| PackageName::from_static(name))
                .collect(),
            package_depends: package_depends
                .iter()
                .map(|name| PackageName::from_static(name))
                .collect(),
        }
    }

    /// A meta package: no source, no build step, just runtime dependencies.
    fn meta(package_depends: &[&'static str]) -> Self {
        Self::new(SourceKind::Meta, BuildKind::None, &[], package_depends)
    }
}

/// A catalog entry. Dependency names must also appear in the same catalog.
#[derive(Clone, Debug)]
pub struct Package {
    pub name: PackageName,
    pub version: &'static str,
    pub description: &'static str,
    pub recipe: Recipe,
}

impl Package {
    fn new(
        name: &'static str,
        version: &'static str,
        description: &'static str,
        recipe: Recipe,
    ) -> Self {
        Self {
            name: PackageName::from_static(name),
            version,
            description,
            recipe,
        }
    }

    /// Runtime dependencies (cookbook's `[package] dependencies`) drive the
    /// install graph.
    #[must_use]
    pub fn runtime_dependencies(&self) -> &[PackageName] {
        &self.recipe.package_depends
    }

    /// Build-time dependencies are informational in `GTos`: it has no in-guest
    /// compiler, but the metadata is kept so a future build backend can use it.
    #[must_use]
    pub fn build_dependencies(&self) -> &[PackageName] {
        &self.recipe.build_depends
    }
}

/// Per-package install state. `manual` distinguishes packages the user asked
/// for from dependencies pulled in automatically, and `dependents` is the
/// reverse edge used to decide whether a package is safe to remove.
#[derive(Clone, Debug, Default)]
pub struct InstallState {
    pub manual: bool,
    pub dependencies: BTreeSet<PackageName>,
    pub dependents: BTreeSet<PackageName>,
}

/// The installed-package dependency graph.
#[derive(Clone, Debug, Default)]
pub struct PackageState {
    /// Packages that cannot be uninstalled, even when nothing depends on them.
    pub protected: BTreeSet<PackageName>,
    /// The installed packages and their graph edges.
    pub installed: BTreeMap<PackageName, InstallState>,
}

impl PackageState {
    /// Resolves `name` and all of its transitive dependencies into `closure`.
    fn collect(
        name: &PackageName,
        catalog: &[Package],
        closure: &mut BTreeSet<PackageName>,
        depth: usize,
    ) -> Result<(), PackageError> {
        if depth > MAX_DEPTH {
            return Err(PackageError::DependencyLimit(name.clone()));
        }
        // A visited set makes cycles terminate rather than recurse forever.
        if closure.contains(name) {
            return Ok(());
        }
        let package = catalog
            .iter()
            .find(|entry| &entry.name == name)
            .ok_or_else(|| PackageError::NotFound(name.clone()))?;
        closure.insert(name.clone());
        for dependency in package.runtime_dependencies() {
            if catalog.iter().all(|entry| &entry.name != dependency) {
                return Err(PackageError::MissingDependency {
                    package: name.clone(),
                    dependency: dependency.clone(),
                });
            }
            Self::collect(dependency, catalog, closure, depth + 1)?;
        }
        Ok(())
    }

    /// Installs the named packages and any missing dependencies.
    ///
    /// Returns the names that were actually added. Requested packages are
    /// marked manual; dependencies pulled in transitively are automatic.
    /// Already-installed packages keep their existing manual flag.
    pub fn install(
        &mut self,
        catalog: &[Package],
        names: &[PackageName],
    ) -> Result<Vec<PackageName>, PackageError> {
        let requested: BTreeSet<PackageName> = names.iter().cloned().collect();
        let mut closure = BTreeSet::new();
        for name in names {
            Self::collect(name, catalog, &mut closure, 0)?;
        }

        let mut added = Vec::new();
        for name in &closure {
            if self.installed.contains_key(name) {
                continue;
            }
            let package = catalog
                .iter()
                .find(|entry| &entry.name == name)
                .ok_or_else(|| PackageError::NotFound(name.clone()))?;
            self.installed.insert(
                name.clone(),
                InstallState {
                    manual: requested.contains(name),
                    dependencies: package.runtime_dependencies().iter().cloned().collect(),
                    dependents: BTreeSet::new(),
                },
            );
            added.push(name.clone());
        }

        // Rebuild reverse edges for every installed package in the closure.
        for name in &closure {
            let dependencies = self
                .installed
                .get(name)
                .map(|state| state.dependencies.clone())
                .unwrap_or_default();
            for dependency in dependencies {
                if let Some(state) = self.installed.get_mut(&dependency) {
                    state.dependents.insert(name.clone());
                }
            }
        }
        Ok(added)
    }

    /// Removes the named packages along with automatic dependencies that would
    /// otherwise be orphaned.
    ///
    /// Fails without changing anything if a package is protected or if a
    /// surviving installed package still depends on one of the targets.
    pub fn uninstall(&mut self, names: &[PackageName]) -> Result<Vec<PackageName>, PackageError> {
        for name in names {
            if self.protected.contains(name) {
                return Err(PackageError::Protected(name.clone()));
            }
        }

        let mut remove: BTreeSet<PackageName> = names.iter().cloned().collect();
        // Expand with automatic dependencies whose every dependent is also
        // being removed. Repeat until the set stops growing.
        loop {
            let mut grew = false;
            let candidates: BTreeSet<PackageName> = remove
                .iter()
                .filter_map(|name| self.installed.get(name))
                .flat_map(|state| state.dependencies.iter().cloned())
                .collect();
            for dependency in candidates {
                if remove.contains(&dependency) || self.protected.contains(&dependency) {
                    continue;
                }
                let Some(state) = self.installed.get(&dependency) else {
                    continue;
                };
                if !state.manual && state.dependents.iter().all(|name| remove.contains(name)) {
                    remove.insert(dependency);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        for name in &remove {
            if let Some(state) = self.installed.get(name) {
                let blockers: Vec<PackageName> = state
                    .dependents
                    .iter()
                    .filter(|dependent| !remove.contains(dependent))
                    .cloned()
                    .collect();
                if !blockers.is_empty() {
                    return Err(PackageError::RequiredBy {
                        package: name.clone(),
                        dependents: blockers,
                    });
                }
            }
        }

        let mut removed = Vec::new();
        for name in &remove {
            if let Some(state) = self.installed.remove(name) {
                for dependency in &state.dependencies {
                    if let Some(dependent) = self.installed.get_mut(dependency) {
                        dependent.dependents.remove(name);
                    }
                }
                removed.push(name.clone());
            }
        }
        Ok(removed)
    }

    /// Marks named packages as manual or automatic. Returns the names changed.
    pub fn mark_as_manual(&mut self, manual: bool, names: &[PackageName]) -> Vec<PackageName> {
        let mut changed = Vec::new();
        for name in names {
            if let Some(state) = self.installed.get_mut(name) {
                if state.manual != manual {
                    state.manual = manual;
                    changed.push(name.clone());
                }
            }
        }
        changed
    }

    /// True if `name` is currently installed.
    #[must_use]
    pub fn is_installed(&self, name: &PackageName) -> bool {
        self.installed.contains_key(name)
    }

    /// Installed names in sorted order.
    #[must_use]
    pub fn installed_list(&self) -> Vec<PackageName> {
        self.installed.keys().cloned().collect()
    }
}

/// The built-in catalog. Each entry carries a cookbook-style recipe: a source,
/// a build kind, build-time dependencies and runtime dependencies. `mathlib`
/// exists so `calculator` demonstrates a transitive dependency that is not
/// installed by default; `desktop` is a meta package like cookbook's `groups`.
pub fn catalog() -> Vec<Package> {
    let builtin =
        |deps: &[&'static str]| Recipe::new(SourceKind::Builtin, BuildKind::Builtin, &[], deps);
    vec![
        Package::new("base", "1.0", "Core GTos runtime", builtin(&[])),
        Package::new(
            "terminal",
            "1.0",
            "Terminal emulator window",
            builtin(&["base"]),
        ),
        Package::new(
            "about",
            "1.0",
            "About GTos information window",
            builtin(&["base"]),
        ),
        Package::new(
            "mycomputer",
            "1.0",
            "System information and drives",
            builtin(&["base"]),
        ),
        Package::new("mydocuments", "1.0", "Document browser", builtin(&["base"])),
        Package::new(
            "browser",
            "1.0",
            "Web browser (HTTP/HTTPS)",
            builtin(&["base"]),
        ),
        Package::new(
            "cmd",
            "1.0",
            "Command prompt window",
            builtin(&["terminal"]),
        ),
        Package::new("mathlib", "1.0", "Calculator runtime library", builtin(&[])),
        Package::new(
            "calculator",
            "1.0",
            "Calculator",
            Recipe::new(
                SourceKind::Builtin,
                BuildKind::Builtin,
                &["mathlib"],
                &["base", "mathlib"],
            ),
        ),
        // Illustrates an archive-sourced package: the metadata names the .pkga
        // on the data disk, and a future backend can unpack it on install.
        Package::new(
            "demo",
            "1.0",
            "Demo program from a .pkga archive",
            Recipe::new(
                SourceKind::Archive("DEMO.PKGA"),
                BuildKind::Archive,
                &[],
                &["base"],
            ),
        ),
        // Meta package: no files of its own, just a dependency group.
        Package::new(
            "desktop",
            "1.0",
            "Meta package: the full desktop",
            Recipe::meta(&[
                "base",
                "terminal",
                "about",
                "mycomputer",
                "mydocuments",
                "browser",
                "cmd",
                "calculator",
            ]),
        ),
    ]
}

/// The default installed set at first use of `pkg`.
pub fn initial_state() -> PackageState {
    let mut state = PackageState::default();
    let catalog = catalog();
    for name in [
        "base",
        "terminal",
        "about",
        "mycomputer",
        "mydocuments",
        "browser",
        "cmd",
    ] {
        state.protected.insert(PackageName::from_static(name));
    }
    let defaults: Vec<PackageName> = [
        "base",
        "terminal",
        "about",
        "mycomputer",
        "mydocuments",
        "browser",
        "cmd",
    ]
    .iter()
    .map(|name| PackageName::from_static(name))
    .collect();
    // Every name is in the catalog and the graph is acyclic, so this cannot fail.
    let _ = state.install(&catalog, &defaults);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str) -> PackageName {
        PackageName::new(value).unwrap()
    }

    #[test_case]
    fn package_names_follow_the_redox_grammar() {
        assert_eq!(name("host:foo.pkg").recipe(), "foo");
        assert_eq!(name("host:foo.pkg").suffix(), Some("pkg"));
        assert!(name("target:foo").is_target());
        assert!(PackageName::new("foo:bar").is_err());
        assert!(PackageName::new("foo.bar.baz").is_err());
        assert!(PackageName::new("dir/foo").is_err());
        assert!(PackageName::new("").is_err());
    }

    #[test_case]
    fn install_resolves_transitive_dependencies() {
        let mut state = PackageState::default();
        let added = state.install(&catalog(), &[name("calculator")]).unwrap();
        assert!(added.contains(&name("calculator")));
        assert!(added.contains(&name("mathlib")));
        assert!(added.contains(&name("base")));
        assert!(state.is_installed(&name("mathlib")));
        assert!(!state.installed[&name("mathlib")].manual);
        assert!(state.installed[&name("calculator")].manual);
        assert!(
            state.installed[&name("mathlib")]
                .dependents
                .contains(&name("calculator"))
        );
    }

    #[test_case]
    fn uninstall_removes_orphaned_automatic_dependencies() {
        let mut state = PackageState::default();
        state.install(&catalog(), &[name("calculator")]).unwrap();
        let removed = state.uninstall(&[name("calculator")]).unwrap();
        assert!(removed.contains(&name("calculator")));
        assert!(removed.contains(&name("mathlib")));
        assert!(!state.is_installed(&name("calculator")));
        assert!(!state.is_installed(&name("mathlib")));
    }

    #[test_case]
    fn uninstall_respects_protected_and_live_dependents() {
        let mut state = initial_state();
        assert_eq!(
            state.uninstall(&[name("terminal")]),
            Err(PackageError::Protected(name("terminal")))
        );
        assert!(matches!(
            state.uninstall(&[name("base")]),
            Err(PackageError::Protected(_))
        ));
        // `calculator` depends on `mathlib`, so removing the latter alone is
        // refused until the dependent is part of the same removal set.
        let mut graph = PackageState::default();
        graph.install(&catalog(), &[name("calculator")]).unwrap();
        assert!(matches!(
            graph.uninstall(&[name("mathlib")]),
            Err(PackageError::RequiredBy { .. })
        ));
        assert!(graph.is_installed(&name("mathlib")));
    }

    #[test_case]
    fn recipes_separate_build_and_runtime_dependencies() {
        let catalog = catalog();
        let calculator = catalog
            .iter()
            .find(|package| package.name == name("calculator"))
            .unwrap();
        assert_eq!(calculator.build_dependencies(), &[name("mathlib")]);
        assert!(calculator.runtime_dependencies().contains(&name("base")));
        assert!(calculator.runtime_dependencies().contains(&name("mathlib")));

        let demo = catalog
            .iter()
            .find(|package| package.name == name("demo"))
            .unwrap();
        assert_eq!(demo.recipe.source, SourceKind::Archive("DEMO.PKGA"));
        assert_eq!(demo.recipe.build, BuildKind::Archive);
    }

    #[test_case]
    fn meta_package_installs_its_group() {
        let mut state = PackageState::default();
        state.install(&catalog(), &[name("desktop")]).unwrap();
        assert!(state.is_installed(&name("calculator")));
        assert!(state.is_installed(&name("mathlib")));
        assert!(!state.installed[&name("mathlib")].manual);
    }

    #[test_case]
    fn mark_as_manual_prevents_orphan_cleanup() {
        let mut state = PackageState::default();
        state.install(&catalog(), &[name("calculator")]).unwrap();
        assert_eq!(
            state.mark_as_manual(true, &[name("mathlib")]),
            vec![name("mathlib")]
        );
        let removed = state.uninstall(&[name("calculator")]).unwrap();
        assert!(!removed.contains(&name("mathlib")));
        assert!(state.is_installed(&name("mathlib")));
    }
}
