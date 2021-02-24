use coco::{state, RunConfig};

use pretty_assertions::assert_eq;

#[macro_use]
mod common;
use common::{build_peer, init_logging, shia_le_pathbuf};

#[tokio::test]
async fn upstream_for_default() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let alice_tmp_dir = tempfile::tempdir()?;
    let alice_repo_path = alice_tmp_dir.path().join("radicle");
    let alice_peer = build_peer(&alice_tmp_dir, RunConfig::default()).await?;
    let alice = state::init_owner(&alice_peer.peer, "alice".to_string()).await?;

    let alice_peer = {
        let peer = alice_peer.peer.clone();
        tokio::task::spawn(alice_peer.into_running());
        peer
    };

    let _ = state::init_project(
        &alice_peer,
        &alice,
        shia_le_pathbuf(alice_repo_path.clone()),
    )
    .await?;

    let repo = git2::Repository::open(alice_repo_path.join("just"))
        .map_err(radicle_surf::vcs::git::error::Error::from)?;
    let remote = repo.branch_upstream_remote("refs/heads/it")?;

    assert_eq!(remote.as_str().unwrap(), "rad");

    let branch = repo.find_branch("rad/it", git2::BranchType::Remote);
    assert!(branch.is_ok(), "could not find `rad/it`");

    Ok(())
}

#[tokio::test]
async fn can_checkout() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let alice_tmp_dir = tempfile::tempdir()?;
    let alice_repo_path = alice_tmp_dir.path().join("radicle");
    let alice_peer = build_peer(&alice_tmp_dir, RunConfig::default()).await?;
    let alice = state::init_owner(&alice_peer.peer, "alice".to_string()).await?;

    let alice_peer = {
        let peer = alice_peer.peer.clone();
        tokio::task::spawn(alice_peer.into_running());
        peer
    };

    let project = state::init_project(
        &alice_peer,
        &alice,
        shia_le_pathbuf(alice_repo_path.clone()),
    )
    .await?;

    let _ = state::checkout(
        &alice_peer,
        project.urn(),
        None,
        alice_repo_path.join("checkout"),
    )
    .await?;

    let _ = state::checkout(
        &alice_peer,
        project.urn(),
        None,
        alice_repo_path.join("checkout"),
    )
    .await?;

    Ok(())
}
