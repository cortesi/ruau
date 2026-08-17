//! Run one file with two mounted roots, then invalidate and prepare it again.

use std::{path::PathBuf, sync::Arc};

use ruau::{
    source::fs::DirectoryMounts,
    surface::{Surface, VmConfig},
    vm::CallOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let user_root = PathBuf::from(args.next().ok_or("expected <user-root>")?);
    let project_root = PathBuf::from(args.next().ok_or("expected <project-root>")?);
    let entry = PathBuf::from(args.next().ok_or("expected <entry-file>")?);

    let mounts = DirectoryMounts::builder()
        .mount("@user", user_root)
        .mount("@project", project_root)
        .build()?;
    let surface = Surface::builder()
        .module_source(Arc::new(mounts.clone()))
        .build()?;
    let mut vm = surface
        .vm_builder(&VmConfig::untrusted(
            ruau::vm::Ambient::deterministic(0),
            ruau::vm::Limits::unlimited(),
        ))
        .build()?;

    let root = mounts.source_for_path(&entry)?;
    let prepared = surface.prepare_graph_ready(root.source().clone())?;
    println!("first: {:?}", prepared.run(&mut vm)?);

    mounts.invalidate_all();
    let refreshed_root = mounts.source_for_path(&entry)?;
    let refreshed = surface.prepare_graph_ready(refreshed_root.source().clone())?;
    println!(
        "refreshed: {:?}",
        refreshed.run_with_options(&mut vm, CallOptions::new())?
    );
    Ok(())
}
