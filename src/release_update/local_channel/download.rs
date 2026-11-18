//! Downloads a local release, verifies it against the pinned key, and unpacks its
//! executables, without changing the active installation.
use super::{LocalChannel, PROJECT, transport, verifier};
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
    release_update::{Release, ReleaseDownload, StagedRelease, download, invalid},
};
use std::{fs, path::Path, time::Duration};

pub struct LocalDownload {
    pub paths: Paths,
    pub channel: LocalChannel,
    pub packslip: verifier::Packslip,
}
impl ReleaseDownload for LocalDownload {
    fn stage(&self, release: &Release, directory: &Path) -> Result<StagedRelease> {
        private_files::directory(directory)?;
        let assets = directory.join("assets");
        private_files::directory(&assets)?;
        let bundle = assets.join("packslip.sigstore.json");
        let archive = assets.join(release.filename());
        let folder = format!("v{}", release.version);
        transport::fetch(
            &self
                .channel
                .url(&format!("{folder}/packslip.sigstore.json")),
            1024 * 1024,
            Some(&bundle),
        )?;
        transport::fetch(
            &self
                .channel
                .url(&format!("{folder}/{}", release.filename())),
            512 * 1024 * 1024,
            Some(&archive),
        )?;
        let verified =
            self.packslip
                .verify(&self.paths, directory, &self.channel.key, &bundle, &archive)?;
        if verified.project != PROJECT
            || verified.version != release.version.to_string()
            || verified.checked_artifacts != [release.filename()]
        {
            return Err(verifier::unverified());
        }
        let unpacked = directory.join("release");
        private_files::directory(&unpacked)?;
        // Only the two named members are extracted, so no path in the archive
        // can reach outside the stage.
        crate::release_update::tools::run(
            Path::new("/usr/bin/tar"),
            &[
                "-xzf",
                archive.to_str().expect("UTF8 stage path"),
                "-C",
                unpacked.to_str().expect("UTF8 stage path"),
                "elsecall",
                "elsecall-ui",
            ],
            directory,
            Duration::from_secs(120),
            &[],
        )
        .map_err(|_| invalid("The verified local release archive could not be unpacked."))?;
        // A selected release keeps its stage; it needs only the executables.
        fs::remove_dir_all(&assets).map_err(|e| Failure::io("Remove release download", e))?;
        download::verified(unpacked.join("elsecall"), release, directory)
    }
}
