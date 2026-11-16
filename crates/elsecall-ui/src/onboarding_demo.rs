//! Explicitly opted-in, isolated native presentation demo. No service access,
//! invitation, installer or credentials; the file lives until the app exits.
use crate::onboarding::{Onboarding, OnboardingPreparation, Prepared, Problem};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

struct Demo {
    file: PathBuf,
    _directory: tempfile::TempDir,
}
impl OnboardingPreparation for Demo {
    fn prepare(&self) -> Result<Prepared, Problem> {
        Ok(Prepared::DemoFile(self.file.clone()))
    }
}

pub fn flow() -> Onboarding {
    let directory = tempfile::Builder::new()
        .prefix("elsecall-airdrop-demo-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("private demo directory");
    let file = directory.path().join("Elsecall AirDrop Demo.txt");
    std::fs::write(&file, "Elsecall AirDrop presentation demo.\nThis harmless file is not an installer or invitation. It does not enroll a Mac.\n").expect("demo file");
    let mut flow = Onboarding::new(
        Arc::new(Demo {
            file,
            _directory: directory,
        }),
        Box::new(crate::airdrop::AirDrop::default()),
    );
    flow.demo = true;
    flow
}
