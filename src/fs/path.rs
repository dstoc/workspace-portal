use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use crate::{
    error::{Error, Result},
    paths,
    state::EntryRecord,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum PortalPath {
    Root,
    Entry { name: String, relative: PathBuf },
}

impl PortalPath {
    pub fn parse(path: impl AsRef<Path>) -> Result<Self> {
        parse_portal_path(path)
    }

    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    pub fn entry_name(&self) -> Option<&str> {
        match self {
            Self::Root => None,
            Self::Entry { name, .. } => Some(name),
        }
    }

    pub fn relative_path(&self) -> Option<&Path> {
        match self {
            Self::Root => None,
            Self::Entry { relative, .. } => Some(relative.as_path()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPortalPath {
    pub entry: EntryRecord,
    pub relative: PathBuf,
    pub target: PathBuf,
}

impl ResolvedPortalPath {
    pub fn is_entry_root(&self) -> bool {
        self.relative.as_os_str().is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenamePlan {
    pub entry: EntryRecord,
    pub source_relative: PathBuf,
    pub target_relative: PathBuf,
    pub source_target: PathBuf,
    pub target_target: PathBuf,
}

pub fn parse_portal_path(path: impl AsRef<Path>) -> Result<PortalPath> {
    let path = path.as_ref();
    let mut parts: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::RootDir => {
                if !parts.is_empty() {
                    return Err(Error::InvalidPortalPath(format!(
                        "embedded root component is not allowed: {}",
                        path.display()
                    )));
                }
            }
            Component::CurDir => {
                return Err(Error::InvalidPortalPath(format!(
                    "current-directory segments are not allowed: {}",
                    path.display()
                )));
            }
            Component::ParentDir => {
                return Err(Error::InvalidPortalPath(format!(
                    "parent-directory traversal is not allowed: {}",
                    path.display()
                )));
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::Prefix(_) => {
                return Err(Error::InvalidPortalPath(format!(
                    "platform-specific path prefixes are not allowed: {}",
                    path.display()
                )));
            }
        }
    }

    if parts.is_empty() {
        return Ok(PortalPath::Root);
    }

    let name = parts.remove(0).into_string().map_err(|_| {
        Error::InvalidPortalPath(format!("non-utf8 entry name: {}", path.display()))
    })?;
    paths::validate_entry_name(&name)?;

    let mut relative = PathBuf::new();
    for part in parts {
        relative.push(part);
    }

    Ok(PortalPath::Entry { name, relative })
}

pub(crate) fn portal_path_to_pathbuf(path: &PortalPath) -> PathBuf {
    match path {
        PortalPath::Root => PathBuf::from("/"),
        PortalPath::Entry { name, relative } => {
            let mut pathbuf = PathBuf::from("/");
            pathbuf.push(name);
            pathbuf.push(relative);
            pathbuf
        }
    }
}

pub(crate) fn parent_portal_path(path: &PortalPath) -> Option<PortalPath> {
    match path {
        PortalPath::Root => None,
        PortalPath::Entry { name, relative } => {
            if relative.as_os_str().is_empty() {
                Some(PortalPath::Root)
            } else {
                let mut parent = relative.clone();
                if !parent.pop() {
                    parent = PathBuf::new();
                }
                Some(PortalPath::Entry {
                    name: name.clone(),
                    relative: parent,
                })
            }
        }
    }
}

pub(crate) fn child_portal_path(parent: &PortalPath, name: &OsString) -> Result<PortalPath> {
    let mut path = portal_path_to_pathbuf(parent);
    path.push(name);
    parse_portal_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_portal_paths() {
        assert_eq!(parse_portal_path("/").unwrap(), PortalPath::Root);

        let path = parse_portal_path("/docs/reports/2026.txt").unwrap();
        assert_eq!(
            path,
            PortalPath::Entry {
                name: "docs".to_owned(),
                relative: PathBuf::from("reports/2026.txt"),
            }
        );

        for invalid in ["docs/..", "../docs", "/docs/../file"] {
            assert!(parse_portal_path(invalid).is_err());
        }
    }
}
