//! Fresh temporary directories for the workspace's tests.
//!
//! Every test that opens a store, a feed directory or a recipe file makes its
//! own directory under the system temp dir. `create_dir_all` is the wrong call
//! for that: it succeeds on a directory an earlier run left behind (a cleanup
//! that raced a still-open SQLite handle leaves one), and Windows recycles
//! process ids eagerly enough for a later run to land on the same name again.
//! That run then reopens the stale database, its rows and ids double, and the
//! test fails on data that is not its own.
//!
//! [`fresh`] never reuses a directory: it is created with `create_dir`, and a
//! name that is already taken gets the next free number appended until a new
//! directory exists.

use std::{
    io,
    path::{
        Path,
        PathBuf,
    },
};

/// Creates a new directory at `candidate` and answers its path.
///
/// On a name that is already taken — a previous run's leftover, or another
/// test that got there first — the name is kept and `-1`, `-2`, … is appended
/// until `create_dir` makes a directory. The answer is therefore always a
/// directory nothing wrote to before, whatever the process id or the leftovers.
///
/// # Errors
///
/// Returns the error `create_dir` answered with anything but "already exists":
/// the parent is missing or is a file, or there is no permission.
pub fn fresh(candidate: &Path) -> io::Result<PathBuf> {
    let mut taken = 0_u64;
    loop {
        let dir = if taken == 0 {
            candidate.to_path_buf()
        } else {
            // The candidate is one name under the temp dir, so a suffix on that
            // name keeps the directory there.
            let name = candidate.file_name().unwrap_or_default().to_string_lossy();
            candidate.with_file_name(format!("{name}-{taken}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => taken += 1,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    // Test assertions may unwrap/expect: fixed synthetic inputs.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amtracker-testdir-{name}-{}", std::process::id()))
    }

    #[test]
    fn creates_the_directory_it_is_asked_for() {
        let dir = fresh(&scratch("plain")).expect("fresh");
        assert!(dir.is_dir(), "{}", dir.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_reuses_a_directory_that_already_exists() {
        let candidate = scratch("taken");
        let name = candidate.file_name().unwrap().to_string_lossy().into_owned();
        let suffixed = |n: u64| candidate.with_file_name(format!("{name}-{n}"));
        let first = fresh(&candidate).expect("fresh");
        // The same name again: a leftover now, so it moves aside.
        let second = fresh(&candidate).expect("fresh again");
        let third = fresh(&candidate).expect("fresh a third time");
        assert_ne!(first, second);
        assert_ne!(second, third);
        // The suffixes count up under the candidate's own name.
        assert_eq!(suffixed(1), second);
        assert_eq!(suffixed(2), third);
        for dir in [first, second, third] {
            assert!(dir.is_dir(), "{}", dir.display());
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn a_file_of_the_same_name_moves_aside_too() {
        let path = scratch("a-file");
        std::fs::write(&path, b"").expect("write");
        // `create_dir` answers "already exists" for a file as for a directory.
        let dir = fresh(&path).expect("fresh");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(path.with_file_name(format!("{name}-1")), dir);
        assert!(dir.is_dir(), "{}", dir.display());
        assert!(path.is_file(), "the file is left alone");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_parent_is_an_error_not_an_endless_retry() {
        let missing = scratch("no-parent").join("nested");
        let error = fresh(&missing).expect_err("no parent");
        assert_eq!(io::ErrorKind::NotFound, error.kind());
    }
}
