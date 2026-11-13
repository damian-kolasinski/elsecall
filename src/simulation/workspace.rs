//! Opaque recorded workspace identities in the virtual address space.
use crate::scheduling::placed::WorkspaceRef;

/// Paths exist only inside the simulator's address space and never touch disk.
pub(super) fn workspace(request: &crate::workload::trace::Request) -> Option<WorkspaceRef> {
    request.workspace.as_ref().map(|w| WorkspaceRef {
        root: format!("/virtual/{}", w.workspace),
        common_dir: format!("/virtual/{}", w.repository),
    })
}
