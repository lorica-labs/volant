// SPDX-License-Identifier: GPL-3.0-or-later
//! `collection init`: a new collection, as `GalaxyCLI.execute_init` of ansible-core 2.19.12
//! writes it, from ansible-core's default skeleton or from a skeleton directory.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{Map, Value, json};

use super::Failure;
use crate::template::Templar;

/// ansible-core 2.19.12's default collection skeleton. `galaxy.yml` is the file it renders from
/// its own metadata for the default values, with the collection's name left to fill in.
const GALAXY_YML: &str = include_str!("skeleton/galaxy.yml.j2");
const README: &str = include_str!("skeleton/README.md.j2");
const PLUGINS_README: &str = include_str!("skeleton/plugins_README.md.j2");
const RUNTIME: &str = include_str!("skeleton/runtime.yml");

/// What `collection init` is given.
pub struct Init<'a> {
    pub name: &'a str,
    pub init_path: &'a str,
    pub force: bool,
    /// `--collection-skeleton`, else `GALAXY_COLLECTION_SKELETON`; ansible-core's own when neither.
    pub skeleton: Option<&'a str>,
    /// `GALAXY_COLLECTION_SKELETON_IGNORE`, read for a skeleton directory only.
    pub ignore: &'a [String],
    pub extra_vars: Map<String, Value>,
}

/// `collection init` as the command line gives it, with the settings of `ansible.cfg` and the
/// environment: `(--init-path, --collection-skeleton)`, `--force` and `--extra-vars`.
pub fn given(
    name: &str,
    (init_path, skeleton): (Option<&str>, Option<&str>),
    force: bool,
    extra_vars: &[String],
    config: &crate::config::Config,
) -> Result<i32, Failure> {
    let env = |name: &str| std::env::var(name).ok();
    let settings = super::settings::Settings::new(&config.galaxy, &env);
    let configured = settings.collection_skeleton();
    let cwd = std::env::current_dir().map_err(|e| io(&e))?;
    let extra_vars = crate::vars::parse_extra_vars(extra_vars, &cwd)
        .map_err(|e| Failure::error(format!("{e:#}")))?;
    collection(&Init {
        name,
        init_path: init_path.unwrap_or("./"),
        force,
        skeleton: skeleton.or(configured.as_deref()),
        ignore: &settings.collection_skeleton_ignore(),
        extra_vars: extra_vars.into_values(),
    })
}

/// Writes the collection under `<init path>/<namespace>/<name>` and says so.
pub fn collection(init: &Init) -> Result<i32, Failure> {
    let Some((namespace, name)) = init
        .name
        .split_once('.')
        .filter(|_| super::requirements::is_collection_name(init.name))
    else {
        return Err(Failure::error(format!(
            "Invalid collection name '{}', name must be in the format <namespace>.<collection>. \nPlease make sure namespace and collection name contains characters from [a-zA-Z0-9_] only.",
            init.name
        )));
    };
    let root = Path::new(init.init_path).join(namespace).join(name);
    if root.exists() {
        if root.is_file() {
            return Err(Failure::error(format!(
                "- the path {} already exists, but is a file - aborting",
                root.display()
            )));
        }
        if !init.force {
            return Err(Failure::error(format!(
                "- the directory {} already exists. You can use --force to re-initialize this directory,\nhowever it will reset any main.yml files that may have\nbeen modified there already.",
                root.display()
            )));
        }
        // The contents go and the directory stays, in case it is the working directory.
        empty(&root).map_err(|e| io(&e))?;
    }
    let skeleton = init
        .skeleton
        .map(|s| crate::config::expand_home(s, Path::new("")));
    if let Some(dir) = skeleton.as_deref().filter(|dir| !dir.exists()) {
        return Err(Failure::error(format!(
            "- the skeleton path '{}' does not exist, cannot init collection",
            dir.display()
        )));
    }
    let mut vars = inject_data(namespace, name);
    vars.extend(init.extra_vars.clone());
    std::fs::create_dir_all(&root).map_err(|e| io(&e))?;
    let written = match &skeleton {
        Some(dir) => {
            let ignore = patterns(init.ignore)?;
            let walk = Walk {
                skeleton: dir,
                root: &root,
                ignore: &ignore,
                vars: &vars,
            };
            walk.dir(Path::new(""))
        }
        None => default_skeleton(&root, &vars),
    };
    if let Err(failed) = written {
        return Err(match failed {
            Written::Io(e) => io(&e),
            Written::Template(source, e) => {
                let _ = std::fs::remove_dir_all(&root);
                Failure::error(format!(
                    "Failed to create Collection {}. Templating {} failed with the error: {e}",
                    init.name,
                    source.display()
                ))
            }
        });
    }
    println!("- Collection {} was created successfully", init.name);
    Ok(0)
}

/// The values ansible-core gives a collection skeleton's templates, before `--extra-vars`.
fn inject_data(namespace: &str, name: &str) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "description": "your collection description",
        "ansible_plugin_list_dir": "https://docs.ansible.com/ansible-core/2.19/plugins/plugins.html",
        "namespace": namespace,
        "collection_name": name,
        "version": "1.0.0",
        "readme": "README.md",
        "authors": ["your name <example@domain.com>"],
        "license": ["GPL-2.0-or-later"],
        "repository": "http://example.com/repository",
        "documentation": "http://docs.example.com",
        "homepage": "http://example.com",
        "issues": "http://example.com/issue/tracker",
        "build_ignore": [],
    }) else {
        unreachable!("a JSON object literal")
    };
    map
}

/// Why a skeleton was not written: a file operation, or a template and its error.
enum Written {
    Io(std::io::Error),
    Template(PathBuf, crate::template::TemplateError),
}

impl From<std::io::Error> for Written {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn io(e: &std::io::Error) -> Failure {
    Failure::unexpected(&e.to_string())
}

/// Removes everything under `dir`, as the reference's `os.walk` over it does with `--force`.
fn empty(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// `text` rendered with `vars`, as a skeleton's `.j2` file is.
fn render(text: &str, vars: &Map<String, Value>) -> Result<String, crate::template::TemplateError> {
    Ok(match Templar::new(PathBuf::from(".")).render(text, vars)? {
        Value::String(s) => s,
        other => other.to_string(),
    })
}

/// ansible-core's own skeleton: its `.git_keep` files are left out, so `docs/` and `roles/` stay
/// empty.
fn default_skeleton(root: &Path, vars: &Map<String, Value>) -> Result<(), Written> {
    let mut galaxy = vars.clone();
    galaxy.insert("name".into(), vars["collection_name"].clone());
    for dir in ["docs", "meta", "plugins", "roles"] {
        std::fs::create_dir_all(root.join(dir))?;
    }
    for (path, text, vars) in [
        ("galaxy.yml", GALAXY_YML, &galaxy),
        ("README.md", README, vars),
        ("plugins/README.md", PLUGINS_README, vars),
    ] {
        let rendered = render(text, vars).map_err(|e| Written::Template(PathBuf::from(path), e))?;
        std::fs::write(root.join(path), rendered)?;
    }
    std::fs::write(root.join("meta/runtime.yml"), RUNTIME)?;
    Ok(())
}

/// `GALAXY_COLLECTION_SKELETON_IGNORE` as `re.match` reads each pattern: anchored at the start.
fn patterns(ignore: &[String]) -> Result<Vec<Regex>, Failure> {
    ignore
        .iter()
        .map(|p| Regex::new(&format!("^(?:{p})")).map_err(|e| Failure::error(e.to_string())))
        .collect()
}

/// A skeleton directory copied into the collection, as the reference's `os.walk` over it copies
/// it: top-down, symbolic links copied as links and never followed into.
struct Walk<'a> {
    skeleton: &'a Path,
    root: &'a Path,
    ignore: &'a [Regex],
    vars: &'a Map<String, Value>,
}

impl Walk<'_> {
    /// The directory `rel` of the skeleton, empty for its top.
    fn dir(&self, rel: &Path) -> Result<(), Written> {
        let source = self.skeleton.join(rel);
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        for entry in std::fs::read_dir(&source)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        dirs.retain(|d| !self.ignore.iter().any(|r| r.is_match(d)));
        let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
        // A collection's `playbooks/*/templates` and `roles/*/templates` hold templates of their
        // own, copied as they are.
        let in_templates = matches!(
            parts.first().map(AsRef::as_ref),
            Some("playbooks" | "roles")
        ) && parts.iter().any(|p| p == "templates");
        for file in files {
            // As `os.path.join(rel_root, f)`, with `.` for the top.
            let shown = if rel.as_os_str().is_empty() {
                format!("./{file}")
            } else {
                format!("{}/{file}", rel.display())
            };
            if self.ignore.iter().any(|r| r.is_match(&shown)) {
                continue;
            }
            let from = source.join(&file);
            match file.strip_suffix(".j2").filter(|_| !in_templates) {
                Some(stem) => {
                    let text = std::fs::read_to_string(&from)?;
                    let rendered =
                        render(&text, self.vars).map_err(|e| Written::Template(from, e))?;
                    std::fs::write(self.root.join(rel).join(stem), rendered)?;
                }
                None => copy(&from, &self.root.join(rel).join(&file))?,
            }
        }
        for dir in &dirs {
            let to = self.root.join(rel).join(dir);
            if to.exists() {
                continue;
            }
            let from = source.join(dir);
            if from.is_symlink() {
                copy(&from, &to)?;
            } else {
                std::fs::create_dir(&to)?;
            }
        }
        for dir in dirs {
            if !source.join(&dir).is_symlink() {
                self.dir(&rel.join(dir))?;
            }
        }
        Ok(())
    }
}

/// `shutil.copyfile(from, to, follow_symlinks=False)`: a link is copied as a link.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if from.is_symlink() {
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
    }
    std::fs::copy(from, to).map(drop)
}
