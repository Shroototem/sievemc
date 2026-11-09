//! Cross-mod side resolution: two questions per mod.
//!
//! 1. **Does another mod require it?** Declared hard dependencies (fabric
//!    `depends`, forge/neoforge required deps, the `modLoader` language
//!    provider) are enforced by the loader, so the required mod must be on
//!    every side the dependent is on. Code use counts too: if a mod's code
//!    calls into a library (or a mod it declares, even as optional), that jar
//!    is needed wherever the caller runs.
//! 2. **Does its own code do anything on the server?** One bytecode walk starts
//!    from the code that loads on a dedicated server (entry points not marked
//!    client-only, mixins not aimed at client code) and follows what it calls.
//!    If it reaches a *server hook* — registering game content, commands or
//!    network packets, server code — or the jar ships data packs or patches
//!    non-client game code, it runs on the server. If not, it's client-only.
//!
//! A mod is on the client unless it's marked server-only, and on the server if
//! question 2 says so or a server mod requires it. Libraries have no side of
//! their own: they go wherever the mods using them go, and a library nothing
//! uses is [`Side::Unused`]. A manifest only ever *restricts* a mod (fabric
//! `environment`, an explicit NeoForge CLIENT/SERVER); `side = "BOTH"` is
//! template boilerplate and is ignored.
//!
//! Performance: jars load in parallel, the class index comes from zip central
//! directories (no decompression), the walk only inflates classes it reaches,
//! and each walk level is parsed in parallel.

use crate::classfile;
use crate::{ModDetection, Side};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use tracing::{debug, info, trace, warn};

type Zip = zip::ZipArchive<Cursor<Arc<[u8]>>>;
/// A class file's identity without inflating it: (entry name, CRC-32, size).
type ContentKey = (String, u32, u64);
/// Where one copy of a class lives: (jar, container, zip entry index).
type EntryLoc = (usize, u16, usize);
/// One walked class: (owning jar, class, referenced classes, mixin targets,
/// content-registering calls).
type Walked = (usize, Box<str>, Vec<String>, Vec<String>, Vec<String>);

/// Mod ids provided by the loader/game itself — never jars in the folder.
const BUILTIN_IDS: &[&str] = &[
    "minecraft",
    "java",
    "fabricloader",
    "fabric-loader",
    "mixinextras",
    "forge",
    "neoforge",
    "javafml",
    "lowcodefml",
];
const MAX_NESTING: u8 = 4;

/// How a requirement was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequirementKind {
    /// Declared as a hard dependency (loader-enforced).
    Depends,
    /// Declared as `recommends`/`suggests` and present in the folder.
    Recommends,
    /// The dependent's code calls into it.
    Bytecode,
}

/// "`by` needs this mod on `side`."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    /// The dependent's mod id (or filename when it has none).
    pub by: String,
    /// The dependent's jar filename.
    pub by_file: String,
    /// `client-only` or `server-only`: which side the requirement applies to.
    pub side: Side,
    pub kind: RequirementKind,
    /// Extra evidence, e.g. `squeek/appleskin/AppleSkinCommon → me/x/Lib`.
    pub detail: Option<String>,
}

/// The dependency-aware verdict for one jar.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// Final side after propagation (`None` = still unsure).
    pub side: Option<Side>,
    /// Whether the jar was treated as a library (side taken from dependents).
    pub library: bool,
    pub required_by: Vec<Requirement>,
    /// Why it got this side, appended to the evidence string.
    pub note: Option<String>,
    pub warnings: Vec<String>,
    /// Set when the resolver read a different manifest than the per-jar pass
    /// (a multi-loader jar in a folder of another loader).
    pub manifest: Option<ManifestOverride>,
}

/// The manifest the resolver used instead of the per-jar pass's choice.
#[derive(Debug, Clone)]
pub struct ManifestOverride {
    pub loader: String,
    pub side: Option<Side>,
    pub evidence: String,
    pub mod_id: Option<String>,
}

/// Which loader's manifest to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Loader {
    Fabric,
    NeoForge,
    Forge,
}

/// What a jar contains before any manifest is parsed.
struct Opened {
    zip: Zip,
    fabric: bool,
    /// `(entry name, loader)` of the forge-style manifest, if any.
    toml: Option<(&'static str, Loader)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Env {
    Any,
    Client,
    Server,
}

struct DepDecl {
    id: String,
    client: bool,
    server: bool,
    hard: bool,
}

struct Jar {
    file: String,
    label: String,
    manifest_side: Option<Side>,
    env: Env,
    library: bool,
    fabric: bool,
    /// Directly provided ids (`false`) and ids provided by nested jars (`true`).
    provides: Vec<(String, bool)>,
    deps: Vec<DepDecl>,
    /// `[0]` is the jar itself, the rest are jar-in-jar containers.
    containers: Vec<Zip>,
    /// Server-side roots: (container, internal class name).
    roots: Vec<(u16, String)>,
    /// Classes from client mixin configs; never walked for the server.
    client_classes: HashSet<String>,
    /// Forge/NeoForge: sides its `@Mod` classes load on (from `dist`), if any.
    entry_side: Option<(bool, bool)>,
    /// Ships `data/` (recipes, worldgen, loot…): server content by definition.
    has_data: bool,
    manifest_override: Option<ManifestOverride>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Loc {
    jar: u32,
    container: u16,
}

fn flags_side(client: bool, server: bool) -> Option<Side> {
    match (client, server) {
        (true, true) => Some(Side::ClientAndServer),
        (true, false) => Some(Side::ClientOnly),
        (false, true) => Some(Side::ServerOnly),
        (false, false) => None,
    }
}

fn label(side: &Option<Side>) -> &'static str {
    match side {
        Some(Side::ClientOnly) => "client",
        Some(Side::ServerOnly) => "server",
        Some(Side::ClientAndServer) => "both",
        Some(Side::Unused) => "unused",
        None => "unsure",
    }
}

fn read_entry(zip: &mut Zip, name: &str) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn open_zip(bytes: Vec<u8>) -> Option<Zip> {
    let bytes: Arc<[u8]> = bytes.into();
    zip::ZipArchive::new(Cursor::new(bytes)).ok()
}

/// `"a.b.C::method"` / `{"value": "a.b.C"}` → `a/b/C`.
fn entrypoint_class(v: &serde_json::Value) -> Option<String> {
    let s = match v {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Object(o) => o.get("value")?.as_str()?,
        _ => return None,
    };
    let class = s.split("::").next()?;
    Some(class.replace('.', "/"))
}

/// A mixin whose every target is a Minecraft client class, or a class owned
/// by a mod proven client-only, can never apply on a dedicated server.
fn mixin_targets_client(
    targets: &[String],
    index: &HashMap<Box<str>, Vec<Loc>>,
    strict_client: &[bool],
) -> bool {
    !targets.is_empty()
        && targets.iter().all(|t| {
            classfile::is_client_target(t)
                || index
                    .get(t.as_str())
                    .is_some_and(|locs| locs.iter().any(|l| strict_client[l.jar as usize]))
        })
}

fn is_client_package(internal: &str) -> bool {
    internal.contains("/client/")
}

/// Phase 1: read a jar into memory and see which manifests it has.
fn open_jar(path: &Path) -> Option<Opened> {
    let zip = open_zip(std::fs::read(path).ok()?)?;
    let has = |n: &str| zip.index_for_name(n).is_some();
    let fabric = has("fabric.mod.json");
    let toml = if has("META-INF/neoforge.mods.toml") {
        Some(("META-INF/neoforge.mods.toml", Loader::NeoForge))
    } else if has("META-INF/mods.toml") {
        Some(("META-INF/mods.toml", Loader::Forge))
    } else {
        None
    };
    Some(Opened { zip, fabric, toml })
}

/// The loader most single-loader jars in the folder use.
fn folder_loader(opened: &[Option<Opened>]) -> Option<Loader> {
    let mut counts: HashMap<Loader, usize> = HashMap::new();
    for o in opened.iter().flatten() {
        let only = match (o.fabric, o.toml) {
            (true, None) => Loader::Fabric,
            (false, Some((_, l))) => l,
            _ => continue,
        };
        *counts.entry(only).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(l, _)| l)
}

fn is_library_mod_type(zip: &mut Zip) -> bool {
    read_entry(zip, "META-INF/MANIFEST.MF")
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .and_then(|mf| {
            mf.lines()
                .find_map(|l| l.strip_prefix("FMLModType:").map(|v| v.trim().to_string()))
        })
        .is_some_and(|t| t == "LIBRARY" || t == "GAMELIBRARY" || t == "LANGPROVIDER")
}

impl Jar {
    /// Phase 2: parse the right manifest for this folder's loader.
    fn build(
        path: &Path,
        manifest_side: Option<Side>,
        opened: Opened,
        preferred: Option<Loader>,
    ) -> Option<Jar> {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Opened { zip, fabric, toml } = opened;
        let mut root = zip.clone();
        let mut jar = Jar {
            label: file.clone(),
            file,
            manifest_side,
            env: Env::Any,
            library: false,
            fabric: false,
            provides: Vec::new(),
            deps: Vec::new(),
            containers: vec![zip],
            roots: Vec::new(),
            client_classes: HashSet::new(),
            entry_side: None,
            has_data: false,
            manifest_override: None,
        };

        // Multi-loader jars: read the manifest of the loader this folder uses.
        let use_toml = toml.is_some() && (!fabric || preferred != Some(Loader::Fabric));
        if fabric && !use_toml {
            let buf = read_entry(&mut root, "fabric.mod.json")?;
            let manifest = crate::parse_json_lenient(&buf).ok()?;
            jar.fabric = true;
            jar.load_fabric(0, &manifest, false, 0);
        } else if let Some((name, loader)) = toml {
            let buf = read_entry(&mut root, name)?;
            let manifest: toml::Value = toml::from_str(std::str::from_utf8(&buf).ok()?).ok()?;
            let loader_name = if loader == Loader::NeoForge {
                "neoforge"
            } else {
                "forge"
            };
            if fabric {
                // The per-jar pass read fabric.mod.json; correct it.
                let side = crate::get_loader_toml_side(manifest.clone(), loader_name).ok();
                let short = name.trim_start_matches("META-INF/");
                jar.manifest_side = side.clone();
                jar.manifest_override = Some(ManifestOverride {
                    loader: loader_name.to_string(),
                    evidence: format!(
                        "{short} → {} (multi-loader jar: using the {loader_name} manifest)",
                        label(&side)
                    ),
                    side,
                    mod_id: manifest
                        .get("mods")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|m| m.get("modId"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                });
            }
            jar.load_forge(Some(&manifest));
        } else {
            // No manifest: a forge-style library wrapper (e.g. kotlinforforge)
            // whose real mods are bundled jar-in-jar.
            let wrapper = is_library_mod_type(&mut root)
                || root
                    .index_for_name("META-INF/jarjar/metadata.json")
                    .is_some();
            if !wrapper {
                return None;
            }
            jar.load_forge(None);
        }
        Some(jar)
    }

    fn load_fabric(&mut self, ci: u16, m: &serde_json::Value, nested: bool, depth: u8) {
        let mut zip = self.containers[ci as usize].clone();
        let env = match m.get("environment").and_then(|v| v.as_str()) {
            Some("client") => Env::Client,
            Some("server") => Env::Server,
            _ => Env::Any,
        };
        if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
            if !nested {
                self.label = id.to_string();
            }
            self.provides.push((id.to_string(), nested));
        }
        if let Some(p) = m.get("provides").and_then(|v| v.as_array()) {
            for id in p.iter().filter_map(|v| v.as_str()) {
                self.provides.push((id.to_string(), nested));
            }
        }

        let dep_keys: &[(&str, bool)] = if nested {
            &[("depends", true)]
        } else {
            &[
                ("depends", true),
                ("recommends", false),
                ("suggests", false),
            ]
        };
        for (key, hard) in dep_keys {
            if let Some(deps) = m.get(*key).and_then(|v| v.as_object()) {
                for id in deps.keys() {
                    if !BUILTIN_IDS.contains(&id.as_str()) {
                        self.deps.push(DepDecl {
                            id: id.clone(),
                            client: true,
                            server: true,
                            hard: *hard,
                        });
                    }
                }
            }
        }

        let entrypoints = m.get("entrypoints").and_then(|v| v.as_object());
        let mixins = m.get("mixins").and_then(|v| v.as_array());
        if env != Env::Client
            && let Some(eps) = entrypoints
        {
            for key in ["main", "server", "preLaunch"] {
                for v in eps
                    .get(key)
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(class) = entrypoint_class(v) {
                        self.roots.push((ci, class));
                    }
                }
            }
        }
        for entry in mixins.into_iter().flatten() {
            let (config, config_env) = match entry {
                serde_json::Value::String(s) => (s.as_str(), None),
                serde_json::Value::Object(o) => (
                    o.get("config").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("environment").and_then(|v| v.as_str()),
                ),
                _ => continue,
            };
            let Some(json) =
                read_entry(&mut zip, config).and_then(|b| crate::parse_json_lenient(&b).ok())
            else {
                continue;
            };
            let package = json
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('.', "/");
            let client_config = config_env == Some("client") || env == Env::Client;
            for (key, client) in [
                ("mixins", client_config),
                ("server", false),
                ("client", true),
            ] {
                for name in json
                    .get(key)
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = name.as_str() else { continue };
                    let class = format!("{package}/{}", name.replace('.', "/"));
                    if client || (key == "server" && client_config) {
                        self.client_classes.insert(class);
                    } else {
                        self.roots.push((ci, class));
                    }
                }
            }
        }

        if !nested {
            self.env = env;
            let has_data = zip.file_names().any(|n| n.starts_with("data/"));
            self.has_data = has_data;
            let badge_library = m
                .pointer("/custom/modmenu/badges")
                .and_then(|v| v.as_array())
                .is_some_and(|b| b.iter().any(|x| x.as_str() == Some("library")));
            let own_code_hooks =
                entrypoints.is_some_and(|e| !e.is_empty()) || mixins.is_some_and(|x| !x.is_empty());
            self.library = badge_library || (!own_code_hooks && !has_data);
        }

        if depth >= MAX_NESTING {
            return;
        }
        // Nested jars are usually deflated (c2me ships ~16 MB of them), so
        // inflate them in parallel; recursion into each stays sequential.
        let files: Vec<&str> = m
            .get("jars")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|j| j.get("file").and_then(|v| v.as_str()))
            .collect();
        let nested: Vec<(Zip, serde_json::Value)> = files
            .par_iter()
            .filter_map(|file| {
                let mut zip = zip.clone();
                let mut inner = read_entry(&mut zip, file).and_then(open_zip)?;
                let manifest = read_entry(&mut inner, "fabric.mod.json")
                    .and_then(|b| crate::parse_json_lenient(&b).ok())?;
                Some((inner, manifest))
            })
            .collect();
        for (inner, manifest) in nested {
            let idx = self.containers.len() as u16;
            self.containers.push(inner);
            self.load_fabric(idx, &manifest, true, depth + 1);
        }
    }

    fn load_forge(&mut self, m: Option<&toml::Value>) {
        let mut zip = self.containers[0].clone();
        let mod_ids: Vec<String> = m
            .and_then(|m| m.get("mods"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.get("modId").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if let Some(first) = mod_ids.first() {
            self.label = first.clone();
        }
        for id in &mod_ids {
            self.provides.push((id.clone(), false));
        }
        if let Some(m) = m {
            self.push_forge_deps(m, &mod_ids, true);
            // `modLoader = "kotlinforforge"` / `"kotori_scala"`: the mod can't
            // load without that language provider jar.
            if let Some(lang) = m.get("modLoader").and_then(|v| v.as_str())
                && !BUILTIN_IDS.contains(&lang)
            {
                self.deps.push(DepDecl {
                    id: lang.to_string(),
                    client: true,
                    server: true,
                    hard: true,
                });
            }
        }
        // Language providers are found by the name mods put in `modLoader`;
        // the jar's Automatic-Module-Name is that name in practice.
        let is_language_provider = [
            "META-INF/services/net.neoforged.neoforgespi.language.IModLanguageLoader",
            "META-INF/services/net.minecraftforge.forgespi.language.IModLanguageLoader",
        ]
        .iter()
        .any(|n| zip.index_for_name(n).is_some());
        if is_language_provider
            && let Some(name) = read_entry(&mut zip, "META-INF/MANIFEST.MF").and_then(|b| {
                String::from_utf8_lossy(&b).lines().find_map(|l| {
                    l.strip_prefix("Automatic-Module-Name:")
                        .map(|v| v.trim().to_string())
                })
            })
        {
            self.provides.push((name, false));
        }

        // Mixins: `[[mixins]]` in the toml, plus Forge's MANIFEST.MF list.
        let mut configs: Vec<String> = m
            .and_then(|m| m.get("mixins"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("config").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if let Some(mf) = read_entry(&mut zip, "META-INF/MANIFEST.MF") {
            let mf = String::from_utf8_lossy(&mf);
            if let Some(list) = mf.lines().find_map(|l| l.strip_prefix("MixinConfigs:")) {
                configs.extend(list.split(',').map(|c| c.trim().to_string()));
            }
        }
        let mut mixin_roots = 0usize;
        for config in configs {
            let Some(json) =
                read_entry(&mut zip, &config).and_then(|b| crate::parse_json_lenient(&b).ok())
            else {
                continue;
            };
            let package = json
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('.', "/");
            for (key, client) in [("mixins", false), ("server", false), ("client", true)] {
                for name in json
                    .get(key)
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = name.as_str() else { continue };
                    let class = format!("{package}/{}", name.replace('.', "/"));
                    if client {
                        self.client_classes.insert(class);
                    } else {
                        self.roots.push((0, class));
                        mixin_roots += 1;
                    }
                }
            }
        }

        // Jar-in-jar: the list lives in META-INF/jarjar/metadata.json (paths
        // aren't always under META-INF/jarjar/).
        let mut nested_files: Vec<String> = read_entry(&mut zip, "META-INF/jarjar/metadata.json")
            .and_then(|b| crate::parse_json_lenient(&b).ok())
            .and_then(|v| v.get("jars").and_then(|j| j.as_array()).cloned())
            .into_iter()
            .flatten()
            .filter_map(|j| j.get("path").and_then(|p| p.as_str()).map(str::to_string))
            .collect();
        if nested_files.is_empty() {
            nested_files = zip
                .file_names()
                .filter(|n| n.starts_with("META-INF/jarjar/") && n.ends_with(".jar"))
                .map(str::to_string)
                .collect();
        }
        let nested: Vec<(Zip, Option<toml::Value>)> = nested_files
            .par_iter()
            .filter_map(|file| {
                let mut zip = zip.clone();
                let mut inner = read_entry(&mut zip, file).and_then(open_zip)?;
                let toml = read_entry(&mut inner, "META-INF/neoforge.mods.toml")
                    .or_else(|| read_entry(&mut inner, "META-INF/mods.toml"))
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|s| toml::from_str::<toml::Value>(&s).ok());
                Some((inner, toml))
            })
            .collect();
        // Containers whose classes can hold entry points: the jar and nested mods.
        let mut mod_containers: Vec<u16> = vec![0];
        for (inner, toml) in nested {
            let ci = self.containers.len() as u16;
            self.containers.push(inner);
            let Some(toml) = toml else { continue };
            let ids: Vec<String> = toml
                .get("mods")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|x| x.get("modId").and_then(|v| v.as_str()).map(str::to_string))
                .collect();
            if self.label == self.file
                && let Some(first) = ids.first()
            {
                self.label = first.clone();
            }
            for id in &ids {
                self.provides.push((id.clone(), true));
            }
            self.push_forge_deps(&toml, &ids, false);
            mod_containers.push(ci);
        }

        // Entry points: @Mod / @EventBusSubscriber classes and their `dist`.
        let candidates: Vec<(u16, String)> = mod_containers
            .iter()
            .flat_map(|&ci| {
                self.containers[ci as usize]
                    .file_names()
                    .filter(|n| n.ends_with(".class") && !n.starts_with("META-INF/"))
                    .map(move |n| (ci, n.to_string()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let containers = &self.containers;
        let entries: Vec<(u16, String, classfile::Entry)> = candidates
            .into_par_iter()
            .filter_map(|(ci, name)| {
                let mut z = containers[ci as usize].clone();
                let bytes = read_entry(&mut z, &name)?;
                let entry = classfile::forge_entry(&bytes)?;
                Some((ci, name.trim_end_matches(".class").to_string(), entry))
            })
            .collect();
        for (ci, class, e) in &entries {
            if e.server {
                self.roots.push((*ci, class.clone()));
            }
        }
        let mods_in = |ci: u16| -> Vec<&classfile::Entry> {
            entries
                .iter()
                .filter(|(c, _, e)| *c == ci && e.kind == classfile::EntryKind::Mod)
                .map(|(_, _, e)| e)
                .collect()
        };
        let mut mods = mods_in(0);
        if mods.is_empty() {
            // Wrapper jars (sodium-neoforge, kotlinforforge) keep their @Mod nested.
            mods = mod_containers[1..]
                .iter()
                .flat_map(|&ci| mods_in(ci))
                .collect();
        }
        if !mods.is_empty() {
            self.entry_side = Some((mods.iter().any(|e| e.client), mods.iter().any(|e| e.server)));
        }
        self.has_data = zip.file_names().any(|n| n.starts_with("data/"));
        // A wrapper with no manifest of its own (kotlinforforge, mezz_config,
        // language providers), or a mod with no entry point, mixins or data:
        // an API other mods call into.
        self.library = m.is_none() || (entries.is_empty() && mixin_roots == 0 && !self.has_data);
    }

    /// The manifest or code says this mod never runs on a dedicated server.
    fn restricted_to_client(&self) -> bool {
        self.env == Env::Client || (!self.fabric && self.manifest_side == Some(Side::ClientOnly))
    }

    /// The manifest or code says this mod never runs on a client.
    fn restricted_to_server(&self) -> bool {
        self.env == Env::Server
            || (!self.fabric
                && (self.manifest_side == Some(Side::ServerOnly)
                    || self.entry_side == Some((false, true))))
    }

    /// Add `[[dependencies.<id>]]` entries for the given mod ids.
    fn push_forge_deps(&mut self, m: &toml::Value, mod_ids: &[String], top_level: bool) {
        for id in mod_ids {
            for dep in m
                .get("dependencies")
                .and_then(|d| d.get(id))
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let Some(dep_id) = dep.get("modId").and_then(|v| v.as_str()) else {
                    continue;
                };
                if BUILTIN_IDS.contains(&dep_id) || mod_ids.iter().any(|x| x == dep_id) {
                    continue;
                }
                let hard = dep
                    .get("mandatory")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || dep
                        .get("type")
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| t.eq_ignore_ascii_case("required"));
                let optional = dep
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t.eq_ignore_ascii_case("optional"));
                // Nested mods' optional deps say nothing about this jar.
                if !hard && (!optional || !top_level) {
                    continue;
                }
                let side = dep.get("side").and_then(|v| v.as_str()).unwrap_or("BOTH");
                self.deps.push(DepDecl {
                    id: dep_id.to_string(),
                    client: !side.eq_ignore_ascii_case("SERVER"),
                    server: !side.eq_ignore_ascii_case("CLIENT"),
                    hard,
                });
            }
        }
    }
}

struct State {
    client: Vec<bool>,
    server: Vec<bool>,
    reqs: Vec<Vec<Requirement>>,
    warnings: Vec<Vec<String>>,
}

impl State {
    /// Record that `jars[by]` needs `target` on `side`; returns true if this
    /// newly turned that side on for `target`.
    fn mark(
        &mut self,
        jars: &[Option<Jar>],
        target: usize,
        by: usize,
        server: bool,
        kind: RequirementKind,
        detail: Option<String>,
    ) -> bool {
        let side = if server {
            Side::ServerOnly
        } else {
            Side::ClientOnly
        };
        let dependent = jars[by].as_ref().expect("dependent jar is loaded");
        if !self.reqs[target]
            .iter()
            .any(|r| r.by_file == dependent.file && r.side == side)
        {
            self.reqs[target].push(Requirement {
                by: dependent.label.clone(),
                by_file: dependent.file.clone(),
                side,
                kind,
                detail,
            });
        }
        let flag = if server {
            &mut self.server[target]
        } else {
            &mut self.client[target]
        };
        let newly = !*flag;
        *flag = true;
        newly
    }
}

/// `from` needs `to` on the flagged sides.
struct Edge {
    to: usize,
    kind: RequirementKind,
    client: bool,
    server: bool,
    detail: Option<String>,
}

/// Pick which jar satisfies a dependency when several provide it (e.g. a
/// fabric-api module bundled by fabric-api *and* by sodium): a top-level jar
/// over a jar-in-jar copy, then a library.
fn pick_provider(candidates: &[(usize, bool)], jars: &[Option<Jar>]) -> Option<usize> {
    candidates
        .iter()
        .min_by_key(|(j, nested)| {
            let lib = jars[*j].as_ref().is_some_and(|x| x.library);
            (*nested, !lib, *j)
        })
        .map(|(j, _)| *j)
}

/// Question 2's definition of "does something on the server": server-side code
/// that reaches one of these (exact class, nested classes included, or a
/// package ending in `/`), or calls one of [`SERVER_METHOD_HOOKS`]. Data packs
/// and mixins into non-client game code count as well.
const SERVER_HOOKS: &[(&str, &str)] = &[
    (
        "com/mojang/brigadier/CommandDispatcher",
        "registers commands",
    ),
    ("net/minecraft/commands/Commands", "registers commands"),
    (
        "net/minecraft/commands/CommandSourceStack",
        "registers commands",
    ),
    (
        "net/neoforged/neoforge/event/RegisterCommandsEvent",
        "registers commands",
    ),
    (
        "net/minecraftforge/event/RegisterCommandsEvent",
        "registers commands",
    ),
    (
        "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback",
        "registers commands",
    ),
    (
        "net/neoforged/neoforge/registries/DeferredRegister",
        "registers game content",
    ),
    (
        "net/neoforged/neoforge/registries/RegisterEvent",
        "registers game content",
    ),
    (
        "net/minecraftforge/registries/DeferredRegister",
        "registers game content",
    ),
    (
        "net/minecraftforge/registries/RegisterEvent",
        "registers game content",
    ),
    // Packets are defined as payload classes, even when a library (Balm,
    // Architectury) does the sending, so this catches the mod's own packets.
    (
        "net/minecraft/network/protocol/common/custom/CustomPacketPayload",
        "uses network packets",
    ),
    ("net/neoforged/neoforge/network/", "uses network packets"),
    ("net/minecraftforge/network/", "uses network packets"),
    (
        "net/fabricmc/fabric/api/networking/v1/PayloadTypeRegistry",
        "uses network packets",
    ),
    (
        "net/fabricmc/fabric/api/networking/v1/ServerPlayNetworking",
        "uses network packets",
    ),
    (
        "net/fabricmc/fabric/api/networking/v1/ServerConfigurationNetworking",
        "uses network packets",
    ),
    ("net/minecraft/server/MinecraftServer", "runs server code"),
    ("net/minecraft/server/level/ServerLevel", "runs server code"),
    (
        "net/minecraft/server/level/ServerPlayer",
        "runs server code",
    ),
    ("net/minecraft/server/network/", "runs server code"),
    ("net/minecraft/server/players/", "runs server code"),
    ("net/neoforged/neoforge/event/server/", "runs server code"),
    (
        "net/neoforged/neoforge/event/tick/ServerTickEvent",
        "runs server code",
    ),
    ("net/minecraftforge/event/server/", "runs server code"),
    (
        "net/fabricmc/fabric/api/event/lifecycle/v1/ServerLifecycleEvents",
        "runs server code",
    ),
    (
        "net/fabricmc/fabric/api/event/lifecycle/v1/ServerTickEvents",
        "runs server code",
    ),
];

/// Calls that register game content (a plain registry *lookup* doesn't count).
const SERVER_METHOD_HOOKS: &[(&str, &str)] = &[
    ("net/minecraft/core/Registry", "register"),
    ("net/minecraft/core/Registry", "registerForHolder"),
    ("net/minecraft/core/Registry", "registerMapping"),
];

fn server_hook(class: &str) -> Option<&'static str> {
    SERVER_HOOKS
        .iter()
        .find(|(p, _)| {
            if p.ends_with('/') {
                class.starts_with(p)
            } else {
                class
                    .strip_prefix(p)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('$'))
            }
        })
        .map(|(_, kind)| *kind)
}

fn server_method_hook(owner: &str, name: &str) -> bool {
    SERVER_METHOD_HOOKS
        .iter()
        .any(|(o, m)| *o == owner && *m == name)
}

/// Spread "needs" along edges to a fixed point.
fn propagate(
    st: &mut State,
    jars: &[Option<Jar>],
    edges: &[Vec<Edge>],
    mut queue: Vec<(usize, bool)>,
) {
    while let Some((j, server)) = queue.pop() {
        for e in &edges[j] {
            let covers = if server { e.server } else { e.client };
            if covers && st.mark(jars, e.to, j, server, e.kind, e.detail.clone()) {
                queue.push((e.to, server));
            }
        }
    }
}

/// Resolve sides for every `(jar path, manifest side)` using the dependency
/// graph of the whole set. Output order matches input order.
pub fn resolve_sides(mods: &[(PathBuf, Option<Side>)]) -> Vec<Resolution> {
    let start = Instant::now();
    let opened: Vec<Option<Opened>> = mods.par_iter().map(|(path, _)| open_jar(path)).collect();
    let preferred = folder_loader(&opened);
    let jars: Vec<Option<Jar>> = mods
        .par_iter()
        .zip(opened.into_par_iter())
        .map(|((path, side), o)| Jar::build(path, side.clone(), o?, preferred))
        .collect();
    let n = jars.len();

    // Class index from central directories only: no decompression.
    let mut index: HashMap<Box<str>, Vec<Loc>> = HashMap::new();
    for (j, jar) in jars.iter().enumerate() {
        let Some(jar) = jar else { continue };
        for (ci, zip) in jar.containers.iter().enumerate() {
            for name in zip.file_names() {
                if let Some(class) = name.strip_suffix(".class") {
                    if class.starts_with("META-INF/") || class.ends_with("module-info") {
                        continue;
                    }
                    index.entry(class.into()).or_default().push(Loc {
                        jar: j as u32,
                        container: ci as u16,
                    });
                }
            }
        }
    }
    let mut providers: HashMap<&str, Vec<(usize, bool)>> = HashMap::new();
    for (j, jar) in jars.iter().enumerate() {
        for (id, nested) in jar.iter().flat_map(|x| x.provides.iter()) {
            providers.entry(id.as_str()).or_default().push((j, *nested));
        }
    }
    trace!(
        "Dependency index: {} classes, {} mod ids, built in {:?}",
        index.len(),
        providers.len(),
        start.elapsed()
    );

    let restrict_client: Vec<bool> = jars
        .iter()
        .map(|j| j.as_ref().is_some_and(Jar::restricted_to_client))
        .collect();
    let restrict_server: Vec<bool> = jars
        .iter()
        .map(|j| j.as_ref().is_some_and(Jar::restricted_to_server))
        .collect();
    // Mods proven client-only: a mixin that only patches these does nothing
    // on a server.
    let strict_client: Vec<bool> = (0..n)
        .map(|j| {
            restrict_client[j]
                || jars[j].as_ref().is_some_and(|x| {
                    !x.library && !x.fabric && x.entry_side == Some((true, false)) && !x.has_data
                })
        })
        .collect();
    let declares = |j: usize, t: usize| -> bool {
        let (Some(a), Some(b)) = (&jars[j], &jars[t]) else {
            return false;
        };
        a.deps
            .iter()
            .any(|d| b.provides.iter().any(|(id, _)| *id == d.id))
    };

    // ---- Question 1 (manifests): declared dependencies ----
    let mut edges: Vec<Vec<Edge>> = (0..n).map(|_| Vec::new()).collect();
    let mut edge_seen: HashSet<(usize, usize)> = HashSet::new();
    let mut missing: Vec<Vec<(String, bool, bool)>> = vec![Vec::new(); n];
    for (j, jar) in jars.iter().enumerate() {
        let Some(jar) = jar else { continue };
        for dep in &jar.deps {
            let Some(cands) = providers.get(dep.id.as_str()) else {
                if dep.hard {
                    missing[j].push((dep.id.clone(), dep.client, dep.server));
                }
                continue;
            };
            if cands.iter().any(|(t, _)| *t == j) {
                continue; // satisfied by its own jar-in-jar
            }
            let Some(t) = pick_provider(cands, &jars) else {
                continue;
            };
            if dep.hard {
                edge_seen.insert((j, t));
                edges[j].push(Edge {
                    to: t,
                    kind: RequirementKind::Depends,
                    client: dep.client,
                    server: dep.server,
                    detail: None,
                });
            } else if jars[t].as_ref().is_some_and(|x| x.library) {
                // An optional library that's present gets used where the mod is.
                edges[j].push(Edge {
                    to: t,
                    kind: RequirementKind::Recommends,
                    client: true,
                    server: false,
                    detail: None,
                });
            }
        }
    }

    // ---- One walk over all server-side code: question 2 + code use ----
    // Every class reached gets a node; the walk records which nodes call
    // which, and which nodes touch a server hook directly. Afterwards one
    // backwards pass credits each hook to every node that leads to it — so a
    // mod doing its server work through a library (Balm, Architectury…) still
    // counts as doing server work.
    let mut node_of: HashMap<(u32, u16, Box<str>), u32> = HashMap::new();
    let mut node_jar: Vec<u32> = Vec::new();
    let mut calls: Vec<Vec<u32>> = Vec::new();
    let mut direct_hook: Vec<Option<String>> = Vec::new();
    let mut roots_of: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut frontier: Vec<(u32, usize, u16, Box<str>)> = Vec::new();
    let mut node = |key: (u32, u16, Box<str>),
                    node_jar: &mut Vec<u32>,
                    calls: &mut Vec<Vec<u32>>,
                    direct_hook: &mut Vec<Option<String>>,
                    frontier: &mut Vec<(u32, usize, u16, Box<str>)>|
     -> u32 {
        if let Some(&id) = node_of.get(&key) {
            return id;
        }
        let id = node_jar.len() as u32;
        node_jar.push(key.0);
        calls.push(Vec::new());
        direct_hook.push(None);
        frontier.push((id, key.0 as usize, key.1, key.2.clone()));
        node_of.insert(key, id);
        id
    };
    for (j, jar) in jars.iter().enumerate() {
        let Some(jar) = jar.as_ref().filter(|_| !restrict_client[j]) else {
            continue;
        };
        for (ci, class) in &jar.roots {
            let id = node(
                (j as u32, *ci, class.as_str().into()),
                &mut node_jar,
                &mut calls,
                &mut direct_hook,
                &mut frontier,
            );
            roots_of[j].push(id);
        }
    }
    let mut parsed = 0usize;
    while !frontier.is_empty() {
        let level = std::mem::take(&mut frontier);
        parsed += level.len();
        let results: Vec<(u32, Walked)> = level
            .into_par_iter()
            .filter_map(|(id, j, ci, class)| {
                let jar = jars[j].as_ref()?;
                if jar.client_classes.contains(&*class) || is_client_package(&class) {
                    return None;
                }
                let mut zip = jar.containers[ci as usize].clone();
                let bytes = read_entry(&mut zip, &format!("{class}.class"))?;
                let info = classfile::parse(
                    &bytes,
                    |r| {
                        server_hook(r).is_some() || (!is_client_package(r) && index.contains_key(r))
                    },
                    server_method_hook,
                )?;
                if info.client_only
                    || mixin_targets_client(&info.mixin_targets, &index, &strict_client)
                {
                    return None;
                }
                Some((
                    id,
                    (j, class, info.refs, info.mixin_targets, info.method_hooks),
                ))
            })
            .collect();

        for (id, (j, class, refs, targets, method_hooks)) in results {
            let here = id as usize;
            if let Some(t) = targets.first() {
                direct_hook[here] = Some(format!("patches server code ({class} → {t})"));
            } else if let Some(call) = method_hooks.first() {
                direct_hook[here] = Some(format!("registers game content ({class} → {call})"));
            }
            for r in refs {
                if direct_hook[here].is_none()
                    && let Some(kind) = server_hook(&r)
                {
                    direct_hook[here] = Some(format!("{kind} ({class} → {r})"));
                }
                let Some(locs) = index.get(r.as_str()) else {
                    continue;
                };
                // Prefer the referencing jar itself (own class or own jar-in-jar).
                let loc = if let Some(own) = locs.iter().find(|l| l.jar as usize == j) {
                    *own
                } else {
                    let cands: Vec<(usize, bool)> = locs
                        .iter()
                        .map(|l| (l.jar as usize, l.container != 0))
                        .collect();
                    let Some(t) = pick_provider(&cands, &jars) else {
                        continue;
                    };
                    if restrict_client[t] {
                        continue; // guarded integration with a client-only mod
                    }
                    // Undeclared calls into a regular mod are almost always
                    // `isModLoaded`-guarded optional compat.
                    let target = jars[t].as_ref().expect("indexed jar is loaded");
                    if !target.library && !declares(j, t) {
                        continue;
                    }
                    if edge_seen.insert((j, t)) {
                        edges[j].push(Edge {
                            to: t,
                            kind: RequirementKind::Bytecode,
                            client: true,
                            server: true,
                            detail: Some(format!("{class} → {r}")),
                        });
                    }
                    *locs
                        .iter()
                        .find(|l| l.jar as usize == t)
                        .expect("provider has a location")
                };
                let child = node(
                    (loc.jar, loc.container, r.as_str().into()),
                    &mut node_jar,
                    &mut calls,
                    &mut direct_hook,
                    &mut frontier,
                );
                calls[here].push(child);
            }
        }
    }

    // Backwards pass: a node leads to a hook if it touches one or calls a node
    // that does. `reaches[node]` = the node whose hook it leads to.
    let mut callers: Vec<Vec<u32>> = vec![Vec::new(); calls.len()];
    for (from, tos) in calls.iter().enumerate() {
        for &to in tos {
            callers[to as usize].push(from as u32);
        }
    }
    let mut reaches: Vec<Option<u32>> = vec![None; calls.len()];
    let mut stack: Vec<u32> = Vec::new();
    for (id, h) in direct_hook.iter().enumerate() {
        if h.is_some() {
            reaches[id] = Some(id as u32);
            stack.push(id as u32);
        }
    }
    // Credit only flows within a jar or out of a library: a library does its
    // work on behalf of whoever calls it, but plugging into another regular
    // mod (a JEI plugin, a FancyMenu add-on) leaves that mod's server work
    // with that mod.
    let is_library: Vec<bool> = jars
        .iter()
        .map(|j| j.as_ref().is_some_and(|x| x.library))
        .collect();
    while let Some(id) = stack.pop() {
        let origin = reaches[id as usize];
        let callee_jar = node_jar[id as usize];
        for &caller in &callers[id as usize] {
            let same_jar = node_jar[caller as usize] == callee_jar;
            if reaches[caller as usize].is_none() && (same_jar || is_library[callee_jar as usize]) {
                reaches[caller as usize] = origin;
                stack.push(caller);
            }
        }
    }
    let hook: Vec<Option<String>> = (0..n)
        .map(|j| {
            let jar = jars[j].as_ref()?;
            if jar.has_data {
                return Some("ships data packs".to_string());
            }
            let origin = roots_of[j].iter().find_map(|&r| reaches[r as usize])? as usize;
            let what = direct_hook[origin].clone()?;
            let owner = node_jar[origin] as usize;
            Some(if owner == j {
                what
            } else {
                let via = jars[owner]
                    .as_ref()
                    .map_or("a library", |x| x.label.as_str());
                format!("{what}, via {via}")
            })
        })
        .collect();

    // ---- Each mod's own sides ----
    let mut st = State {
        client: vec![false; n],
        server: vec![false; n],
        reqs: vec![Vec::new(); n],
        warnings: vec![Vec::new(); n],
    };
    let mut reasons: Vec<Option<String>> = vec![None; n];
    for j in 0..n {
        if jars[j].as_ref().is_none_or(|x| x.library) {
            continue;
        }
        let (c, s, why) = if restrict_server[j] {
            (false, true, "it's marked server-only".to_string())
        } else if restrict_client[j] {
            (true, false, "it's marked client-only".to_string())
        } else if let Some(h) = &hook[j] {
            (true, true, format!("does work on the server: {h}"))
        } else {
            (
                true,
                false,
                "nothing it runs on a server touches the server".to_string(),
            )
        };
        st.client[j] = c;
        st.server[j] = s;
        reasons[j] = Some(format!("{why} → {}", label(&flags_side(c, s))));
    }

    // ---- Question 1: spread requirements along the edges ----
    let queue: Vec<(usize, bool)> = (0..n)
        .flat_map(|j| {
            let mut v = Vec::new();
            if st.client[j] {
                v.push((j, false));
            }
            if st.server[j] {
                v.push((j, true));
            }
            v
        })
        .collect();
    propagate(&mut st, &jars, &edges, queue);

    // ---- Unused libraries: prove the client doesn't need them either ----
    // A library no manifest and no server-side code requires is a candidate for
    // dropping. Client-only code was never walked, so before dropping, scan the
    // constant pools of every client-installed jar for a reference to a
    // candidate's classes (undeclared client-side use). Any hit keeps it on the
    // client, and its own deps follow. Repeat until nothing changes.
    let mut scanned = vec![false; n];
    let mut seen_content: HashSet<ContentKey> = HashSet::new();
    // A library whose every class is also bundled inside another jar is a
    // duplicate: the loader serves one copy to everybody, so nothing can depend
    // on this jar specifically. No scan needed to drop it.
    let duplicate_of: Vec<Option<String>> = (0..n)
        .map(|t| {
            let jar = jars[t].as_ref().filter(|x| x.library)?;
            if st.client[t] || st.server[t] || !st.reqs[t].is_empty() {
                return None;
            }
            let mut other: Option<usize> = None;
            let mut any = false;
            for zip in &jar.containers {
                for class in zip.file_names().filter_map(|f| f.strip_suffix(".class")) {
                    if class.starts_with("META-INF/") || class.ends_with("module-info") {
                        continue;
                    }
                    any = true;
                    let elsewhere =
                        index.get(class)?.iter().find(|l| l.jar as usize != t)?.jar as usize;
                    other.get_or_insert(elsewhere);
                }
            }
            if !any {
                return None;
            }
            jars[other?].as_ref().map(|x| x.label.clone())
        })
        .collect();
    loop {
        let candidate: Vec<bool> = (0..n)
            .map(|j| {
                jars[j].as_ref().is_some_and(|x| x.library)
                    && duplicate_of[j].is_none()
                    && !st.client[j]
                    && !st.server[j]
                    && st.reqs[j].is_empty()
            })
            .collect();
        let remaining = candidate.iter().filter(|c| **c).count();
        let to_scan: Vec<usize> = (0..n)
            .filter(|&j| !scanned[j] && st.client[j] && jars[j].is_some())
            .collect();
        if remaining == 0 || to_scan.is_empty() {
            break;
        }
        // Cheap pre-filter before any hash lookup: the package roots (first two
        // segments) of every candidate class. Derived from the candidates' own
        // class names, so it can't cause a false negative.
        let mut prefixes: Vec<String> = Vec::new();
        for (j, jar) in jars.iter().enumerate() {
            let Some(jar) = jar.as_ref().filter(|_| candidate[j]) else {
                continue;
            };
            for zip in &jar.containers {
                for f in zip.file_names().filter_map(|f| f.strip_suffix(".class")) {
                    // `a/b/c/X` → `a/b/`, `a/X` → `a/`, `X` → `X`.
                    let cut = f
                        .match_indices('/')
                        .nth(1)
                        .or_else(|| f.match_indices('/').next_back())
                        .map_or(f.len(), |(i, _)| i + 1);
                    if !prefixes.iter().any(|p| f.starts_with(p.as_str())) {
                        prefixes.push(f[..cut].to_string());
                    }
                }
            }
        }
        let found: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
        let found_count = AtomicUsize::new(0);
        // Group identical class files (same name, CRC and size, read from the
        // central directory without inflating) so each is scanned once. Packs
        // bundle the same fabric-api modules / libs inside many mods.
        let mut groups: HashMap<ContentKey, Vec<EntryLoc>> = HashMap::new();
        for &j in &to_scan {
            scanned[j] = true;
            let jar = jars[j].as_ref().expect("filtered to loaded jars");
            for (ci, zip) in jar.containers.iter().enumerate() {
                let mut zip = zip.clone();
                for i in 0..zip.len() {
                    let Ok(entry) = zip.by_index_raw(i) else {
                        continue;
                    };
                    let name = entry.name();
                    if !name.ends_with(".class") || name.starts_with("META-INF/") {
                        continue;
                    }
                    let key = (name.to_string(), entry.crc32(), entry.size());
                    if seen_content.contains(&key) {
                        continue; // already proven not to reference any candidate
                    }
                    groups.entry(key).or_default().push((j, ci as u16, i));
                }
            }
        }
        parsed += groups.len();
        let groups: Vec<(ContentKey, Vec<EntryLoc>)> = groups.into_iter().collect();
        let hits: Vec<(usize, usize, String)> = groups
            .par_iter()
            .filter_map(|((file, _, _), copies)| {
                if found_count.load(Ordering::Relaxed) >= remaining {
                    return None; // every candidate is already known to be used
                }
                let (j0, ci0, i0) = copies[0];
                let mut zip = jars[j0].as_ref()?.containers[ci0 as usize].clone();
                let mut entry = zip.by_index(i0).ok()?;
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes).ok()?;
                let refs = classfile::referenced_classes(&bytes, |r| {
                    prefixes.iter().any(|p| r.starts_with(p.as_str()))
                        && index.get(r).is_some_and(|locs| {
                            locs.iter().any(|l| {
                                candidate[l.jar as usize]
                                    && !found[l.jar as usize].load(Ordering::Relaxed)
                            })
                        })
                });
                let mut out = Vec::new();
                for r in refs {
                    let locs = &index[r.as_str()];
                    for &(j, _, _) in copies {
                        // A class the jar bundles itself (jar-in-jar copy) doesn't count.
                        if locs.iter().any(|l| l.jar as usize == j) {
                            continue;
                        }
                        for l in locs {
                            let t = l.jar as usize;
                            if candidate[t] && !found[t].swap(true, Ordering::Relaxed) {
                                found_count.fetch_add(1, Ordering::Relaxed);
                                let class = file.trim_end_matches(".class");
                                out.push((t, j, format!("{class} → {r}")));
                            }
                        }
                    }
                }
                Some(out)
            })
            .flatten()
            .collect();
        seen_content.extend(groups.into_iter().map(|(k, _)| k));
        if hits.is_empty() {
            break;
        }
        let mut queue = Vec::new();
        for (t, by, detail) in hits {
            if st.mark(&jars, t, by, false, RequirementKind::Bytecode, Some(detail)) {
                queue.push((t, false));
            }
        }
        propagate(&mut st, &jars, &edges, queue);
    }

    // Missing hard dependencies only matter on a side the mod is actually on.
    for (j, missing) in missing.into_iter().enumerate() {
        for (id, c, s) in missing {
            if (c && st.client[j]) || (s && st.server[j]) {
                st.warnings[j].push(format!("depends on `{id}`, which is not in this folder"));
            }
        }
    }

    // ---- Final verdicts ----
    let mut changed = 0usize;
    let out: Vec<Resolution> = (0..n)
        .map(|j| {
            let Some(jar) = &jars[j] else {
                return Resolution {
                    side: mods[j].1.clone(),
                    ..Default::default()
                };
            };
            let mut warnings = std::mem::take(&mut st.warnings[j]);
            let reqs = std::mem::take(&mut st.reqs[j]);
            let (mut c, mut s) = (st.client[j], st.server[j]);

            let unused = jar.library && reqs.is_empty();
            if unused {
                (c, s) = (false, false);
            }
            if restrict_client[j] && s {
                let by: Vec<&str> = reqs
                    .iter()
                    .filter(|r| r.side == Side::ServerOnly)
                    .map(|r| r.by.as_str())
                    .collect();
                warnings.push(format!(
                    "required on the server by {}, but it's marked client-only",
                    by.join(", ")
                ));
                s = false;
            }
            if restrict_server[j] && c {
                warnings.push("required on the client, but it's marked server-only".to_string());
                c = false;
            }

            let side = if unused {
                Some(Side::Unused)
            } else {
                flags_side(c, s).or_else(|| jar.manifest_side.clone())
            };
            let mut note = if unused {
                match &duplicate_of[j] {
                    Some(other) => format!(
                        "unused library: {other} already bundles its own copy of everything in it → drop"
                    ),
                    None => "unused library: nothing in this folder requires it → drop".to_string(),
                }
            } else if jar.library {
                format!("library: goes where the mods using it go → {}", label(&side))
            } else {
                reasons[j].take().unwrap_or_default()
            };
            if !reqs.is_empty() {
                let mut why: Vec<String> = reqs
                    .iter()
                    .take(4)
                    .map(|r| {
                        let on = if r.side == Side::ServerOnly { "server" } else { "client" };
                        let kind = match r.kind {
                            RequirementKind::Depends => "depends",
                            RequirementKind::Recommends => "recommends",
                            RequirementKind::Bytecode => "uses it",
                        };
                        format!("{on}: {} ({kind})", r.by)
                    })
                    .collect();
                if reqs.len() > 4 {
                    why.push(format!("+{} more", reqs.len() - 4));
                }
                note = format!("{note} · required by {}", why.join(", "));
            }
            if side != jar.manifest_side {
                changed += 1;
                info!(
                    "{}: {} → {} ({})",
                    jar.file,
                    label(&jar.manifest_side),
                    label(&side),
                    note
                );
            }
            for r in &reqs {
                debug!(
                    "{} required on {:?} by {} via {:?}{}",
                    jar.file,
                    r.side,
                    r.by,
                    r.kind,
                    r.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()
                );
            }
            for w in &warnings {
                warn!("{}: {w}", jar.file);
            }
            Resolution {
                side,
                library: jar.library,
                required_by: reqs,
                note: Some(note),
                warnings,
                manifest: jar.manifest_override.clone(),
            }
        })
        .collect();

    info!(
        "Dependency resolution: {} jars, {} classes parsed, {} sides changed in {:?}",
        n,
        parsed,
        changed,
        start.elapsed()
    );
    out
}

/// Apply [`resolve_sides`] to detections in place, updating `side`,
/// `required_by`, `library`, `warnings` and appending to `evidence`.
pub fn apply_dependency_sides(detections: &mut [ModDetection]) {
    let input: Vec<(PathBuf, Option<Side>)> = detections
        .iter()
        .map(|d| (d.path.clone(), d.side.clone()))
        .collect();
    for (d, r) in detections.iter_mut().zip(resolve_sides(&input)) {
        if let Some(m) = r.manifest {
            d.loader = Some(m.loader);
            d.manifest_side = m.side;
            d.evidence = m.evidence;
            if m.mod_id.is_some() {
                d.mod_id = m.mod_id;
            }
        }
        d.side = r.side;
        d.library = r.library;
        d.required_by = r.required_by;
        d.warnings = r.warnings;
        if let Some(note) = r.note {
            d.evidence = format!("{} · {note}", d.evidence);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_dir(dir: &str) -> HashMap<String, ModDetection> {
        crate::detect_mods(dir)
            .unwrap()
            .into_iter()
            .map(|d| (d.mod_id.clone().unwrap(), d))
            .collect()
    }

    #[test]
    fn server_bytecode_promotes_client_flagged_library() {
        let m = resolve_dir("../../examples/deps");
        let lib = &m["configlib"];
        assert_eq!(lib.manifest_side, Some(Side::ClientOnly));
        assert_eq!(lib.side, Some(Side::ClientAndServer));
        assert!(lib.required_by.iter().any(|r| r.by == "servermod"
            && r.side == Side::ServerOnly
            && r.kind == RequirementKind::Bytecode));
    }

    #[test]
    fn library_chains_follow_bytecode() {
        let m = resolve_dir("../../examples/deps");
        assert_eq!(m["deeplib"].side, Some(Side::ClientAndServer));
        assert!(m["deeplib"].required_by.iter().any(|r| r.by == "configlib"));
    }

    #[test]
    fn client_annotated_code_does_not_pull_library_onto_server() {
        let m = resolve_dir("../../examples/deps");
        // uilib is only used from an @Environment(CLIENT) method (and its lambda).
        assert_eq!(m["uilib"].side, Some(Side::ClientOnly));
    }

    #[test]
    fn unused_library_is_dropped_entirely() {
        let m = resolve_dir("../../examples/deps");
        // orphanlib looks like architectury: library badge, main + server
        // entrypoints, but nothing in the folder depends on it or uses it.
        let orphan = &m["orphanlib"];
        assert_eq!(orphan.manifest_side, Some(Side::ClientAndServer));
        assert!(orphan.required_by.is_empty());
        assert_eq!(orphan.side, Some(Side::Unused));
        assert!(orphan.evidence.contains("unused library"));
        // A dropped library's own dependencies don't count as requirements.
        assert!(!m["uilib"].required_by.iter().any(|r| r.by == "orphanlib"));
        assert_eq!(m["uilib"].side, Some(Side::ClientOnly));
    }

    #[test]
    fn undeclared_client_use_keeps_library_on_client() {
        let m = resolve_dir("../../examples/deps");
        // clientmod calls hiddenlib without declaring it anywhere.
        let lib = &m["hiddenlib"];
        assert_eq!(lib.side, Some(Side::ClientOnly));
        assert!(lib.required_by.iter().any(|r| r.by == "clientmod"
            && r.side == Side::ClientOnly
            && r.kind == RequirementKind::Bytecode));
    }

    fn resolve_files(dir: &str) -> HashMap<String, ModDetection> {
        crate::detect_mods(dir)
            .unwrap()
            .into_iter()
            .map(|d| (d.filename.trim_end_matches(".jar").to_string(), d))
            .collect()
    }

    #[test]
    fn neoforge_mod_with_server_hook_is_both() {
        // Boilerplate side = "BOTH"; its @Mod registers a command.
        let m = resolve_files("../../examples/deps-neoforge");
        assert_eq!(m["neo_cmd"].side, Some(Side::ClientAndServer));
        assert!(m["neo_cmd"].evidence.contains("registers commands"));
    }

    #[test]
    fn neoforge_mod_doing_nothing_server_side_is_client() {
        // Boilerplate side = "BOTH", @Mod loads on both sides but touches no
        // server hook.
        let m = resolve_files("../../examples/deps-neoforge");
        assert_eq!(m["neo_idle"].manifest_side, Some(Side::ClientAndServer));
        assert_eq!(m["neo_idle"].side, Some(Side::ClientOnly));
    }

    #[test]
    fn neoforge_client_dist_code_never_runs_on_server() {
        // Its @Mod would register a command, but dist = CLIENT keeps it off
        // the server entirely.
        let m = resolve_files("../../examples/deps-neoforge");
        assert_eq!(m["neo_clientdist"].side, Some(Side::ClientOnly));
    }

    #[test]
    fn method_references_are_followed() {
        // `Runner.run(Setup::init)`: Setup is only referenced from the
        // invokedynamic bootstrap arguments.
        let m = resolve_files("../../examples/deps-neoforge");
        assert_eq!(m["neo_methodref"].side, Some(Side::ClientAndServer));
        assert!(m["neo_methodref"].evidence.contains("neomethodref/Setup"));
    }

    #[test]
    fn language_provider_follows_modloader_users() {
        let m = resolve_files("../../examples/deps-neoforge");
        let lang = &m["neo_lang"];
        assert!(lang.library);
        assert_eq!(lang.side, Some(Side::ClientAndServer));
        assert!(lang.required_by.iter().any(|r| r.by == "neo_scripted"
            && r.side == Side::ServerOnly
            && r.kind == RequirementKind::Depends));
    }

    #[test]
    fn unused_library_wrapper_is_dropped() {
        let m = resolve_files("../../examples/deps-neoforge");
        assert_eq!(m["neo_unusedlib"].side, Some(Side::Unused));
    }

    #[test]
    fn manifest_with_raw_line_break_is_read() {
        // Strict JSON rejects the raw line break in its description; Fabric
        // (and now sievemc) accept it.
        let m = resolve_dir("../../examples/deps");
        let mod_ = &m["multiline"];
        assert_eq!(mod_.manifest_side, Some(Side::ClientAndServer));
        assert_eq!(mod_.side, Some(Side::ClientAndServer));
        assert!(mod_.evidence.contains("registers game content"));
    }

    #[test]
    fn undeclared_content_mod_reference_is_optional() {
        let m = resolve_dir("../../examples/deps");
        assert!(m["othermod"].required_by.is_empty());
        assert_eq!(m["othermod"].side, Some(Side::ClientAndServer));
    }
}
