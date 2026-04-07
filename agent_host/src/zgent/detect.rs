use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

pub fn zgent_root_dir() -> PathBuf {
    repository_root().join("zgent")
}

pub fn zgent_runtime_available() -> bool {
    zgent_root_dir().is_dir()
}

pub fn zgent_server_binary_candidates(root_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for base in [
        root_dir.join("target"),
        root_dir
            .parent()
            .map(|parent| parent.join("target"))
            .unwrap_or_else(|| root_dir.join("target")),
    ] {
        candidates.push(base.join("release/zgent-server"));
        candidates.push(base.join("debug/zgent-server"));
    }
    candidates
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZgentNativeKernelReadiness {
    MissingRuntimeDir {
        root_dir: PathBuf,
    },
    SourceOnly {
        root_dir: PathBuf,
        manifest_path: PathBuf,
    },
    Ready {
        root_dir: PathBuf,
        server_binary: PathBuf,
    },
}

impl ZgentNativeKernelReadiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub fn summary(&self) -> String {
        match self {
            Self::MissingRuntimeDir { root_dir } => format!(
                "local ./zgent runtime directory is unavailable at {}",
                root_dir.display()
            ),
            Self::SourceOnly {
                root_dir,
                manifest_path,
            } => format!(
                "local ./zgent source tree exists at {} but no built zgent-server binary was found; discovered manifest at {}",
                root_dir.display(),
                manifest_path.display()
            ),
            Self::Ready {
                root_dir,
                server_binary,
            } => format!(
                "local ./zgent runtime at {} is ready with built server binary {}",
                root_dir.display(),
                server_binary.display()
            ),
        }
    }
}

pub fn zgent_native_kernel_readiness() -> ZgentNativeKernelReadiness {
    let root_dir = zgent_root_dir();
    if !root_dir.is_dir() {
        return ZgentNativeKernelReadiness::MissingRuntimeDir { root_dir };
    }

    for server_binary in zgent_server_binary_candidates(&root_dir) {
        if server_binary.is_file() {
            return ZgentNativeKernelReadiness::Ready {
                root_dir,
                server_binary,
            };
        }
    }

    let manifest_path = root_dir.join("Cargo.toml");
    if manifest_path.is_file() {
        return ZgentNativeKernelReadiness::SourceOnly {
            root_dir,
            manifest_path,
        };
    }

    ZgentNativeKernelReadiness::MissingRuntimeDir { root_dir }
}

#[cfg(test)]
mod tests {
    use super::{
        ZgentNativeKernelReadiness, zgent_native_kernel_readiness, zgent_root_dir,
        zgent_runtime_available, zgent_server_binary_candidates,
    };
    use std::path::Path;

    #[test]
    fn zgent_root_path_points_to_repo_level_directory() {
        let root = zgent_root_dir();
        assert_eq!(root.file_name().and_then(|v| v.to_str()), Some("zgent"));
        assert!(root.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("..")));
    }

    #[test]
    fn availability_matches_directory_presence() {
        assert_eq!(zgent_runtime_available(), zgent_root_dir().is_dir());
    }

    #[test]
    fn native_kernel_readiness_matches_basic_filesystem_state() {
        let readiness = zgent_native_kernel_readiness();
        match readiness {
            ZgentNativeKernelReadiness::MissingRuntimeDir { root_dir } => {
                assert!(!root_dir.is_dir());
            }
            ZgentNativeKernelReadiness::SourceOnly {
                root_dir,
                manifest_path,
            } => {
                assert!(root_dir.is_dir());
                assert!(manifest_path.is_file());
            }
            ZgentNativeKernelReadiness::Ready {
                root_dir,
                server_binary,
            } => {
                assert!(root_dir.is_dir());
                assert!(server_binary.is_file());
            }
        }
    }

    #[test]
    fn binary_candidates_include_repo_and_workspace_target_dirs() {
        let root = zgent_root_dir();
        let candidates = zgent_server_binary_candidates(&root);
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("zgent/target/release/zgent-server"))
        );
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("target/release/zgent-server"))
        );
    }
}
