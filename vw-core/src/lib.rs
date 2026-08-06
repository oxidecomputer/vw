// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw-core`: low-level VHDL analysis and nvc-runner primitives shared by
//! `vw-lib` (the workflow layer) and `anodizer` (VHDL-record code generation).
//!
//! This crate deliberately has no dependency on git/deps-resolution or any
//! other workflow machinery so that `anodizer` can depend on it without
//! creating a cycle back into `vw-lib`.

use std::collections::{hash_map::Entry, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use serde::{Deserialize, Serialize};
use vhdl_lang::{VHDLParser, VHDLStandard};

use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
};

use crate::mapping::{FileData, SymbolKind, VwSymbol, VwSymbolFinder};
use crate::nvc_helpers::run_nvc_analysis;
use crate::visitor::walk_design_file;

pub mod mapping;
pub mod nvc_helpers;
pub mod visitor;

// ============================================================================
// Error Types
// ============================================================================

#[derive(Debug)]
pub enum VwError {
    Config { message: String },
    Dependency { message: String },
    Git { message: String },
    FileSystem { message: String },
    Testbench { message: String },
    NvcSimulation { command: String },
    NvcElab { command: String },
    NvcAnalysis { library: String, command: String },
    CodeGen { message: String },
    Simulation { message: String },
    Io(std::io::Error),
    Serialization(toml::ser::Error),
    Deserialization(toml::de::Error),
    Regex(regex::Error),
}

impl std::error::Error for VwError {}
impl From<std::io::Error> for VwError {
    fn from(err: std::io::Error) -> Self {
        VwError::Io(err)
    }
}

impl From<toml::ser::Error> for VwError {
    fn from(err: toml::ser::Error) -> Self {
        VwError::Serialization(err)
    }
}

impl From<toml::de::Error> for VwError {
    fn from(err: toml::de::Error) -> Self {
        VwError::Deserialization(err)
    }
}

impl From<regex::Error> for VwError {
    fn from(err: regex::Error) -> Self {
        VwError::Regex(err)
    }
}

pub type Result<T> = std::result::Result<T, VwError>;

impl fmt::Display for VwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VwError::NvcSimulation { command } => {
                writeln!(f, "NVC simulation failed")?;
                writeln!(f, "command:")?;
                writeln!(f, "{command}")?;
                Ok(())
            }
            VwError::NvcElab { command } => {
                writeln!(f, "NVC elaboration failed")?;
                writeln!(f, "command:")?;
                writeln!(f, "{command}")?;
                Ok(())
            }
            VwError::NvcAnalysis { library, command } => {
                writeln!(f, "NVC analysis failed for library '{library}'")?;
                writeln!(f, "command:")?;
                writeln!(f, "{command}")?;
                Ok(())
            }
            VwError::CodeGen { message } => {
                write!(f, "Code generation failed: {message}")
            }
            VwError::Simulation { message } => {
                write!(f, "Simulation error: {message}")
            }
            VwError::Config { message } => {
                write!(f, "Configuration error: {message}")
            }
            VwError::Dependency { message } => {
                write!(f, "Dependency error: {message}")
            }
            VwError::Git { message } => {
                write!(f, "Git operation failed: {message}")
            }
            VwError::FileSystem { message } => {
                write!(f, "File system error: {message}")
            }
            VwError::Testbench { message } => {
                write!(f, "Testbench error: {message}")
            }
            VwError::Io(e) => write!(f, "IO error: {e}"),
            VwError::Serialization(e) => write!(f, "Serialization error: {e}"),
            VwError::Deserialization(e) => {
                write!(f, "Deserialization error: {e}")
            }
            VwError::Regex(e) => write!(f, "Regex error: {e}"),
        }
    }
}

// ============================================================================
// VHDL Standard
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub enum VhdlStandard {
    Vhdl2008,
    Vhdl2019,
}

impl From<VhdlStandard> for VHDLStandard {
    fn from(val: VhdlStandard) -> Self {
        match val {
            VhdlStandard::Vhdl2008 => VHDLStandard::VHDL2008,
            VhdlStandard::Vhdl2019 => VHDLStandard::VHDL2019,
        }
    }
}

/// The inverse of [`VhdlStandard`]'s `Display`.
///
/// Paired with it deliberately: the standard is written into command lines and
/// now also sent to an instance that has to turn it back into the same value,
/// and a spelling that only goes one way is a spelling that eventually
/// disagrees with itself.
impl std::str::FromStr for VhdlStandard {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim() {
            "2008" => Ok(VhdlStandard::Vhdl2008),
            "2019" => Ok(VhdlStandard::Vhdl2019),
            other => Err(format!(
                "'{other}' is not a vhdl standard vw knows; expected 2008 or \
                 2019"
            )),
        }
    }
}

impl fmt::Display for VhdlStandard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VhdlStandard::Vhdl2008 => write!(f, "2008"),
            VhdlStandard::Vhdl2019 => write!(f, "2019"),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct VhdlLsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    pub libraries: HashMap<String, VhdlLsLibrary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<HashMap<String, serde_json::Value>>,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VhdlLsLibrary {
    pub files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_third_party: Option<bool>,
}
pub struct RecordProcessor {
    pub vhdl_std: VhdlStandard,
    pub symbols: HashMap<String, VwSymbol>,
    pub symbol_to_file: HashMap<String, String>,
    pub tagged_names: HashSet<String>,
    pub file_info: HashMap<String, FileData>,
    pub target_attr: String,
}

const RECORD_PARSE_ATTRIBUTE: &str = "serialize_rust";
impl RecordProcessor {
    pub fn new(std: VhdlStandard) -> Self {
        Self {
            vhdl_std: std,
            symbols: HashMap::new(),
            symbol_to_file: HashMap::new(),
            tagged_names: HashSet::new(),
            file_info: HashMap::new(),
            target_attr: RECORD_PARSE_ATTRIBUTE.to_string(),
        }
    }
}

// ============================================================================
// File Cache - Reduces redundant file reads during build
// ============================================================================

/// Cache for parsed file data to avoid redundant parsing during builds.
/// Only caches parsed results, not raw file contents.
pub struct FileCache {
    dependencies: HashMap<PathBuf, Vec<VwSymbol>>,
    provided_symbols: HashMap<PathBuf, Vec<VwSymbol>>,
    entities: HashMap<PathBuf, Vec<String>>,
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            dependencies: HashMap::new(),
            provided_symbols: HashMap::new(),
            entities: HashMap::new(),
        }
    }

    /// Get cached file dependencies, reading and parsing file if not cached.
    pub fn get_dependencies(&mut self, path: &Path) -> Result<&Vec<VwSymbol>> {
        match self.dependencies.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let deps = parse_file_dependencies(&content)?;
                Ok(e.insert(deps))
            }
        }
    }

    /// Get cached provided symbols (packages and entities), reading and parsing if not cached.
    pub fn get_provided_symbols(
        &mut self,
        path: &Path,
    ) -> Result<&Vec<VwSymbol>> {
        match self.provided_symbols.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let symbols = parse_provided_symbols(&content)?;
                Ok(e.insert(symbols))
            }
        }
    }

    /// Get cached entities in file, reading and parsing if not cached.
    pub fn get_entities(&mut self, path: &Path) -> Result<&Vec<String>> {
        match self.entities.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let entities = parse_entities(&content)?;
                Ok(e.insert(entities))
            }
        }
    }

    /// Get mutable access to the entities cache for functions that only need entity lookups.
    pub fn entities_cache_mut(&mut self) -> &mut HashMap<PathBuf, Vec<String>> {
        &mut self.entities
    }
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse dependencies from file content (extracted for use by FileCache).
fn parse_file_dependencies(content: &str) -> Result<Vec<VwSymbol>> {
    let mut dependencies = Vec::new();
    let mut seen = HashSet::new();

    // Package imports from "use work.package_name"
    let imports = get_package_imports(content)?;
    for pkg in imports {
        let key = format!("pkg:{}", pkg.to_lowercase());
        if seen.insert(key) {
            dependencies.push(VwSymbol::new(None, &pkg, SymbolKind::Package));
        }
    }

    // Find direct entity instantiations (instance_name: entity work.entity_name)
    let entity_inst_pattern = r"(?i)\w+\s*:\s*entity\s+work\.(\w+)";
    let entity_inst_re = regex::Regex::new(entity_inst_pattern)?;

    for captures in entity_inst_re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            let name = entity_name.as_str().to_string();
            let key = format!("ent:{}", name.to_lowercase());
            if seen.insert(key) {
                dependencies.push(VwSymbol::new(
                    None,
                    &name,
                    SymbolKind::Entity,
                ));
            }
        }
    }

    // Find component declarations
    let comp_decl_pattern = r"(?i)component\s+(\w+)";
    let comp_decl_re = regex::Regex::new(comp_decl_pattern)?;

    for captures in comp_decl_re.captures_iter(content) {
        if let Some(comp_name) = captures.get(1) {
            let name = comp_name.as_str().to_string();
            let key = format!("ent:{}", name.to_lowercase());
            if seen.insert(key) {
                dependencies.push(VwSymbol::new(
                    None,
                    &name,
                    SymbolKind::Entity,
                ));
            }
        }
    }

    Ok(dependencies)
}

/// Parse provided symbols (packages and entities) from file content.
fn parse_provided_symbols(content: &str) -> Result<Vec<VwSymbol>> {
    let mut symbols = Vec::new();

    // Find package declarations
    let package_pattern = r"(?i)\bpackage\s+(\w+)\s+is\b";
    let package_re = regex::Regex::new(package_pattern)?;

    for captures in package_re.captures_iter(content) {
        if let Some(package_name) = captures.get(1) {
            symbols.push(VwSymbol::new(
                None,
                package_name.as_str(),
                SymbolKind::Package,
            ));
        }
    }

    // Find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let entity_re = regex::Regex::new(entity_pattern)?;

    for captures in entity_re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            symbols.push(VwSymbol::new(
                None,
                entity_name.as_str(),
                SymbolKind::Entity,
            ));
        }
    }

    Ok(symbols)
}

/// Parse entity declarations from file content.
pub fn parse_entities(content: &str) -> Result<Vec<String>> {
    let mut entities = Vec::new();

    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let re = regex::Regex::new(entity_pattern)?;

    for captures in re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            entities.push(entity_name.as_str().to_string());
        }
    }

    Ok(entities)
}

pub async fn analyze_ext_libraries(
    vhdl_ls_config: &VhdlLsConfig,
    processor: &mut RecordProcessor,
    vhdl_std: VhdlStandard,
    build_dir: &str,
    cache: &mut FileCache,
) -> Result<()> {
    // Collect non-defaultlib library names
    let ext_lib_names: Vec<String> = vhdl_ls_config
        .libraries
        .keys()
        .filter(|k| k.as_str() != "defaultlib")
        .cloned()
        .collect();

    // Build inter-library dependency graph by scanning for `library <name>;`
    let ext_lib_set: HashSet<String> = ext_lib_names.iter().cloned().collect();
    let mut lib_deps: HashMap<String, Vec<String>> = HashMap::new();
    for lib_name in &ext_lib_names {
        let mut deps = Vec::new();
        if let Some(library) = vhdl_ls_config.libraries.get(lib_name) {
            for file_path in &library.files {
                let expanded = if file_path.starts_with("$HOME") {
                    if let Some(home) = dirs::home_dir() {
                        home.join(
                            file_path
                                .strip_prefix("$HOME/")
                                .unwrap_or(file_path),
                        )
                    } else {
                        PathBuf::from(file_path)
                    }
                } else {
                    PathBuf::from(file_path)
                };
                if let Ok(contents) = fs::read_to_string(&expanded) {
                    for line in contents.lines() {
                        let trimmed = line.trim().to_lowercase();
                        if let Some(rest) = trimmed.strip_prefix("library ") {
                            let dep_lib = rest.trim_end_matches(';').trim();
                            if ext_lib_set.contains(dep_lib)
                                && dep_lib != lib_name.to_lowercase()
                            {
                                deps.push(dep_lib.to_string());
                            }
                        }
                    }
                }
            }
        }
        lib_deps.insert(lib_name.clone(), deps);
    }

    // Topological sort of library names (Kahn's algorithm)
    let mut in_degree: HashMap<String, usize> =
        ext_lib_names.iter().map(|n| (n.clone(), 0)).collect();
    let mut adj: HashMap<String, Vec<String>> = ext_lib_names
        .iter()
        .map(|n| (n.clone(), Vec::new()))
        .collect();
    for (lib, deps) in &lib_deps {
        for dep in deps {
            if let Some(neighbors) = adj.get_mut(dep) {
                neighbors.push(lib.clone());
            }
            if let Some(deg) = in_degree.get_mut(lib) {
                *deg += 1;
            }
        }
    }
    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(n, _)| n.clone())
        .collect();
    let mut sorted_libs = Vec::new();
    while let Some(current) = queue.pop_front() {
        sorted_libs.push(current.clone());
        if let Some(neighbors) = adj.get(&current) {
            for neighbor in neighbors {
                if let Some(deg) = in_degree.get_mut(neighbor) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }
    }
    // Fall back to unsorted if cycle detected
    if sorted_libs.len() != ext_lib_names.len() {
        sorted_libs = ext_lib_names;
    }

    // Analyze libraries in dependency order
    for lib_name in &sorted_libs {
        if let Some(library) = vhdl_ls_config.libraries.get(lib_name) {
            // Convert library name to be NVC-compatible (no hyphens)
            let nvc_lib_name = lib_name.replace('-', "_");

            let mut files = Vec::new();
            for file_path in &library.files {
                // Convert $HOME paths to absolute paths
                let expanded_path = if file_path.starts_with("$HOME") {
                    let home_dir = dirs::home_dir().ok_or_else(|| {
                        VwError::FileSystem {
                            message: "Could not determine home directory"
                                .to_string(),
                        }
                    })?;
                    home_dir.join(
                        file_path.strip_prefix("$HOME/").unwrap_or(file_path),
                    )
                } else {
                    PathBuf::from(file_path)
                };
                files.push(expanded_path);
            }

            // Sort files in dependency order (dependencies first)
            sort_files_by_dependencies(processor, &mut files, cache)?;

            let file_strings: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();

            run_nvc_analysis(
                vhdl_std,
                build_dir,
                &nvc_lib_name,
                &file_strings,
                false,
            )
            .await?;
        }
    }

    Ok(())
}
pub fn find_referenced_files(
    testbench_file: &Path,
    available_files: &[PathBuf],
    cache: &mut FileCache,
) -> Result<Vec<PathBuf>> {
    let mut referenced_files = Vec::new();
    let mut processed_files = HashSet::new();
    let mut files_to_process = vec![testbench_file.to_path_buf()];

    while let Some(current_file) = files_to_process.pop() {
        if processed_files.contains(&current_file) {
            continue;
        }
        processed_files.insert(current_file.clone());

        // Don't include the testbench file itself in the referenced files
        // (it will be added separately)
        if current_file != testbench_file {
            referenced_files.push(current_file.clone());
        }

        let dependencies = cache.get_dependencies(&current_file)?.clone();

        // Find corresponding files for each dependency
        for dep in dependencies {
            for available_file in available_files {
                if file_provides_symbol(available_file, &dep, cache)? {
                    if !processed_files.contains(available_file) {
                        files_to_process.push(available_file.clone());
                    }
                    break;
                }
            }
        }
    }

    Ok(referenced_files)
}

pub fn sort_files_by_dependencies(
    processor: &mut RecordProcessor,
    files: &mut Vec<PathBuf>,
    cache: &mut FileCache,
) -> Result<()> {
    // Build dependency graph
    let mut dependencies: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut all_symbols: HashMap<String, PathBuf> = HashMap::new();

    // First pass: collect all symbols provided by each file
    for file in files.iter() {
        let symbols = analyze_file(processor, file)?;
        for symbol in symbols {
            match &symbol.kind {
                SymbolKind::Package => {
                    all_symbols.insert(symbol.name.clone(), file.clone());
                    let entry = processor
                        .file_info
                        .entry(file.to_string_lossy().to_string())
                        .or_default();
                    entry.add_defined_pkg(&symbol.name);

                    // Use cache to get package imports only
                    let deps = cache.get_dependencies(file)?;
                    for dep in deps {
                        if let SymbolKind::Package = dep.kind {
                            entry.add_imported_pkg(&dep.name);
                        }
                    }
                }
                SymbolKind::Entity => {
                    all_symbols.insert(symbol.name, file.clone());
                }
                _ => {}
            }
        }
    }

    // Second pass: find dependencies for each file
    for file in files.iter() {
        let deps = cache.get_dependencies(file)?.clone();
        let mut file_deps = Vec::new();

        for dep in deps {
            let dep_name = match &dep.kind {
                SymbolKind::Package | SymbolKind::Entity => &dep.name,
                _ => continue,
            };
            if let Some(provider_file) = all_symbols.get(dep_name) {
                if provider_file != file {
                    file_deps.push(provider_file.clone());
                }
            }
        }

        dependencies.insert(file.clone(), file_deps);
    }

    // Topological sort using Kahn's algorithm
    let sorted = topological_sort_files(files.clone(), dependencies)?;
    *files = sorted;

    Ok(())
}
// ============================================================================
// Internal Helper Functions
// ============================================================================

fn get_package_imports(content: &str) -> Result<Vec<String>> {
    // Find 'use work.package_name' statements
    let use_work_pattern = r"(?i)use\s+work\.(\w+)";
    let use_work_re = regex::Regex::new(use_work_pattern)?;
    let mut imports = Vec::new();

    for captures in use_work_re.captures_iter(content) {
        if let Some(package_name) = captures.get(1) {
            imports.push(package_name.as_str().to_string());
        }
    }
    Ok(imports)
}

fn file_provides_symbol(
    file_path: &Path,
    needed: &VwSymbol,
    cache: &mut FileCache,
) -> Result<bool> {
    let provided = cache.get_provided_symbols(file_path)?;
    Ok(provided.iter().any(|s| match (&needed.kind, &s.kind) {
        // Package dependency matches package declaration
        (SymbolKind::Package, SymbolKind::Package) => {
            needed.name.eq_ignore_ascii_case(&s.name)
        }
        // Entity dependency matches entity declaration
        (SymbolKind::Entity, SymbolKind::Entity) => {
            needed.name.eq_ignore_ascii_case(&s.name)
        }
        _ => false,
    }))
}

fn analyze_file(
    processor: &mut RecordProcessor,
    file: &Path,
) -> Result<Vec<VwSymbol>> {
    let parser = VHDLParser::new(processor.vhdl_std.into());
    let mut diagnostics = Vec::new();
    let (_, design_file) = parser.parse_design_file(file, &mut diagnostics)?;

    let mut file_finder = VwSymbolFinder::new(&processor.target_attr);
    walk_design_file(&mut file_finder, &design_file);

    let file_str = file.to_string_lossy().to_string();

    // Add symbols to the map

    for symbol in file_finder.get_symbols() {
        match symbol.kind {
            SymbolKind::Enum(_)
            | SymbolKind::Record(_)
            | SymbolKind::Constant(_) => {
                let name = symbol.get_name().to_string();
                processor.symbols.insert(name.clone(), symbol.clone());
                processor.symbol_to_file.insert(name, file_str.clone());
            }
            _ => {}
        }
    }

    for tagged_type in file_finder.get_tagged_types() {
        processor.tagged_names.insert(tagged_type.clone());
    }

    Ok(file_finder.get_symbols().clone())
}

fn topological_sort_files(
    files: Vec<PathBuf>,
    dependencies: HashMap<PathBuf, Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let mut dep_graph: DiGraph<PathBuf, ()> = DiGraph::default();
    let mut index_map: HashMap<PathBuf, NodeIndex> = HashMap::new();

    // initialize the nodes
    for file in &files {
        let index = dep_graph.add_node(file.clone());
        index_map.insert(file.clone(), index);
    }

    // now add edges from files to their dependencies
    for (file, deps) in &dependencies {
        let source_node = index_map.get(file).ok_or(VwError::Dependency {
            message: format!(
                "Index map somehow didn't contain file {:?}",
                file
            ),
        })?;
        // file depends on every dep in deps
        for dep in deps {
            let dst_node = index_map.get(dep).ok_or(VwError::Dependency {
                message: format!(
                    "Index map somehow didn't contain dep {:?}",
                    dep
                ),
            })?;
            dep_graph.add_edge(*source_node, *dst_node, ());
        }
    }

    // ok now topological sort
    let ordered_files =
        toposort(&dep_graph, None).map_err(|_| VwError::Dependency {
            message: "Got circular dependency".to_string(),
        })?;

    let result: Vec<PathBuf> = ordered_files
        .iter()
        .map(|&idx| dep_graph[idx].clone())
        .rev()
        .collect();
    Ok(result)
}

/// Parse an existing `vhdl_ls.toml` file into a [`VhdlLsConfig`].
///
/// A pure, disk-reading loader with no workspace/deps resolution — suitable
/// for the standalone `anodizer` CLI. The `vw` workflow renders a config in
/// memory instead (see `vw_lib::render_vhdl_ls_config`) and hands it to
/// `anodizer` directly.
pub fn load_existing_vhdl_ls_config(path: &Path) -> Result<VhdlLsConfig> {
    let contents = fs::read_to_string(path)?;
    let config: VhdlLsConfig = toml::from_str(&contents)?;
    Ok(config)
}
