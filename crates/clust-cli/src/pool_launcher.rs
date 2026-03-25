use std::io;
use std::process::{Command, Stdio};

/// Spawn `clust-pool` as a detached background process.
pub fn spawn_pool() -> io::Result<()> {
    // Resolve clust-pool binary path relative to the current executable.
    // During development (cargo build/run), both binaries are in the same
    // target/{debug,release}/ directory. After `cargo install`, both are
    // in ~/.cargo/bin/.
    let current_exe = std::env::current_exe()?;
    let bin_dir = current_exe
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine bin directory"))?;
    let pool_bin = bin_dir.join("clust-pool");

    if !pool_bin.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("clust-pool binary not found at {}", pool_bin.display()),
        ));
    }

    let mut cmd = Command::new(&pool_bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Detach into own process group so it survives terminal close
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn()?;

    Ok(())
}
