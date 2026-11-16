//! The companion window shipped beside a CLI release and staged by installation.
use crate::{build_identity::Digest, failure::Failure, paths::Paths, private_files};
use std::{fs, path::Path};

/// `elsecall-ui` sitting next to the service executable that is being installed.
pub fn companion(source: &Path) -> std::path::PathBuf {
    source.with_file_name("elsecall-ui")
}

/// Copies the sibling window into the support directory when the install has one.
/// A missing sibling leaves any previously stored window in place.
pub fn store_companion(paths: &Paths, source: &Path) -> crate::failure::Result<()> {
    let sibling = companion(source);
    if !sibling.is_file() {
        return Ok(());
    }
    let bytes = fs::read(&sibling).map_err(|error| Failure::io("Read the fleet window", error))?;
    private_files::write(&paths.window_executable(), &bytes, 0o700)
}

pub fn installed_build(
    paths: &Paths,
) -> crate::failure::Result<Option<crate::build_identity::BuildId>> {
    let path = paths.window_executable();
    if !path.is_file() {
        return Ok(None);
    }
    let digest =
        Digest::of_file(&path).map_err(|error| Failure::io("Read the fleet window", error))?;
    Ok(Some(digest.build_id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_sibling_window_is_stored_and_hashed() {
        let home = std::path::PathBuf::from(format!("/tmp/ec-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let paths = Paths::for_home(&home).unwrap();
        fs::create_dir_all(&paths.data).unwrap();
        let source_dir = home.join("build");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("elsecall");
        fs::write(&source, b"service").unwrap();
        store_companion(&paths, &source).unwrap();
        assert!(!paths.window_executable().exists());
        fs::write(source_dir.join("elsecall-ui"), b"window-bytes").unwrap();
        store_companion(&paths, &source).unwrap();
        assert_eq!(
            fs::read(paths.window_executable()).unwrap(),
            b"window-bytes"
        );
        let build = installed_build(&paths).unwrap().unwrap();
        assert_eq!(build, Digest::of(b"window-bytes").build_id());
        let _ = fs::remove_dir_all(&home);
    }
}
