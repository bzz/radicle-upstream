use librad::signer::BoxedSigner;
use radicle_surf::vcs::git::Tag;

#[derive(Debug, Clone)]
pub struct MergeRequest {
    pub id: String,
    pub merged: bool,
    pub peer: crate::project::Peer<crate::project::peer::Status<crate::Person>>,
    pub message: Option<String>,
    pub commit: git2::Oid,
}

/// TODO
///
/// # Errors
pub async fn list(
    peer: &crate::net::peer::Peer<BoxedSigner>,
    project: crate::Urn,
) -> Result<Vec<MergeRequest>, crate::state::Error> {
    let mut merge_requests = Vec::new();
    let monorepo_path = crate::state::monorepo(peer);
    let monorepo = git2::Repository::open(monorepo_path)?;
    let namespace = librad::git::types::namespace::Namespace::from(project.clone());

    for project_peer in crate::state::list_project_peers(peer, project.clone()).await? {
        let remote = match project_peer {
            crate::project::Peer::Local { .. } => None,
            crate::project::Peer::Remote { peer_id, .. } => Some(peer_id),
        };
        let ref_pattern = librad::git::types::Reference {
            remote: remote,
            category: librad::git::types::RefsCategory::Tags,
            name: librad::refspec_pattern!("merge-request/*"),
            namespace: Some(namespace.clone()),
        };
        let refs = ref_pattern.references(&monorepo)?;
        for r in refs {
            let r = r?;
            let tag = monorepo.find_tag(r.target().unwrap())?;
            let id = tag.name().unwrap().strip_prefix("merge-request/").unwrap();
            assert_eq!(tag.target_type(), Some(git2::ObjectType::Commit));
            merge_requests.push(MergeRequest {
                id: id.to_owned(),
                merged: false,
                peer: project_peer.clone(),
                message: tag.message().map(String::from),
                commit: tag.target_id(),
            })
        }
    }
    Ok(merge_requests)
}
