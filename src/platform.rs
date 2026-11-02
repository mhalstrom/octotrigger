//! What differs between Unix (macOS, Linux) and native Windows: the home
//! folder, a file only the person can read, a file lock, the process tree of
//! one agent, and the shell that runs a command line of the setup.
//!
//! An agent's process tree: on Unix the agent runs in a process group of its
//! own, and a stop kills the group. On Windows the agent runs in a job object
//! of its own: a stop ends the job, which ends every process in it, and the
//! job also ends when octotrigger's handle closes, so an agent never outlives
//! the octotrigger that started it.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// The person's home folder: `HOME` on Unix, `USERPROFILE` on Windows (a
/// `HOME` that Git Bash sets can name another folder).
#[must_use]
pub fn home() -> PathBuf {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Makes a file (mode 600) or a folder (mode 700) readable by the person
/// only. On Windows it changes nothing: a file under the person's profile
/// folder inherits the profile's access list, which lets in only the person,
/// the system and the administrators.
///
/// # Errors
///
/// A path whose mode cannot be set.
pub fn make_private(path: &Path, folder: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if folder { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, folder);
        Ok(())
    }
}

/// An exclusive lock on an open file, held until it is dropped.
pub struct FileLock(File);

impl FileLock {
    /// Waits for the exclusive lock of `file`.
    ///
    /// # Errors
    ///
    /// A lock the system refuses.
    pub fn take(file: File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: flock takes the descriptor of a file this value owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
            // SAFETY: a zeroed OVERLAPPED is offset 0 with no event.
            let mut overlapped = unsafe { std::mem::zeroed() };
            // SAFETY: the handle is of a file this value owns; the OVERLAPPED
            // lives until the call returns, which waits for the lock.
            if unsafe {
                LockFileEx(
                    file.as_raw_handle(),
                    LOCKFILE_EXCLUSIVE_LOCK,
                    0,
                    u32::MAX,
                    u32::MAX,
                    &mut overlapped,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: as in take; the file stays open until after this call.
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
            // SAFETY: as in take.
            unsafe {
                let mut overlapped = std::mem::zeroed();
                UnlockFileEx(
                    self.0.as_raw_handle(),
                    0,
                    u32::MAX,
                    u32::MAX,
                    &mut overlapped,
                );
            }
        }
    }
}

/// Sets up a command so that its process starts a tree of its own: a
/// process group on Unix; on Windows no console window (its output goes to
/// a file).
pub fn own_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// The process tree of one started agent.
pub struct Tree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the job handle is a kernel handle, usable from any thread.
#[cfg(windows)]
unsafe impl Send for Tree {}

impl Tree {
    /// The tree of a child started with [`own_tree`]. On Windows, a job
    /// object that ends every process in it when it closes; a process the
    /// agent starts joins the job.
    ///
    /// # Errors
    ///
    /// On Windows, a job object the system does not make or join.
    pub fn of(child: &Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { pid: child.id() })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            };
            // SAFETY: no attributes and no name: a new, unnamed job.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a zeroed limit structure is a valid "no limits" value.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX);
            // SAFETY: the job is ours; the structure lives across the calls;
            // the child's handle stays open while `child` lives.
            let joined = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    size,
                ) != 0
                    && AssignProcessToJobObject(job, child.as_raw_handle()) != 0
            };
            if !joined {
                let error = io::Error::last_os_error();
                // SAFETY: the job is ours and is not used again.
                unsafe { CloseHandle(job) };
                return Err(error);
            }
            Ok(Self { job })
        }
    }

    /// Kills the agent and every process it started.
    pub fn kill(&self) {
        #[cfg(unix)]
        kill_group(self.pid);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: the job is ours and open until drop.
            unsafe { TerminateJobObject(self.job, 1) };
        }
    }
}

#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: the job is ours; closing it ends what is left in it.
        unsafe { CloseHandle(self.job) };
    }
}

/// Kills the tree of an agent that an earlier octotrigger started, by its
/// process id. On Windows it does nothing: the earlier octotrigger's job
/// closed when that process ended, and that ended its agents; a kill by id
/// could hit another program that now has the same id.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: killpg sends a signal; it reads and writes no memory of ours.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Whether a process still lives.
#[must_use]
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(pid).is_ok_and(|pid| {
            // SAFETY: signal 0 checks the process and sends nothing.
            unsafe { libc::kill(pid, 0) == 0 }
        })
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess returns a handle we close, or null.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut code = 0u32;
        // SAFETY: the handle is open; `code` lives across the call.
        let read = unsafe { GetExitCodeProcess(process, &mut code) } != 0;
        // SAFETY: the handle is ours and is not used again.
        unsafe { CloseHandle(process) };
        #[allow(clippy::cast_sign_loss)]
        let still = STILL_ACTIVE as u32;
        read && code == still
    }
}

/// A command line of the setup, in the person's shell: `/bin/sh -c` on Unix,
/// Windows PowerShell on Windows.
#[must_use]
pub fn shell(line: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", line]);
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(line);
        command
    }
}

/// The program a command names: a path (with a leading `~/` read as the
/// home folder) or a name on the PATH. On Windows a name with no extension
/// is tried with `.exe`, `.cmd` and `.bat`, as the Windows shell does.
/// `None` when there is no such file.
#[must_use]
pub fn find_program(word: &str) -> Option<PathBuf> {
    let path = crate::config::expand(Path::new(word));
    let candidates = |base: &Path| -> Vec<PathBuf> {
        let mut found = vec![base.to_path_buf()];
        if cfg!(windows) && base.extension().is_none() {
            for extension in ["exe", "cmd", "bat"] {
                found.push(base.with_extension(extension));
            }
        }
        found
    };
    let has_folder = path.components().count() > 1 || path.is_absolute();
    if has_folder {
        return candidates(&path).into_iter().find(|p| p.is_file());
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .flat_map(|folder| candidates(&folder.join(&path)))
        .find(|p| p.is_file())
}

/// The words of a Windows command line, as Windows programs read them
/// (`CommandLineToArgvW`): double quotes group a word, and a backslash is
/// a plain character unless quotes follow it.
///
/// # Errors
///
/// A line that has no word.
#[cfg(windows)]
pub fn split_words(line: &str) -> io::Result<Vec<String>> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
    let line = line.trim();
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the agent command is empty",
        ));
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(line)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut count = 0i32;
    // SAFETY: `wide` ends with 0 and lives across the call; the answer is
    // freed below.
    let words = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
    if words.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    for index in 0..usize::try_from(count).unwrap_or(0) {
        // SAFETY: Windows gave `count` pointers, each to a string that ends
        // with 0.
        let word = unsafe { *words.add(index) };
        let mut length = 0;
        // SAFETY: as above.
        while unsafe { *word.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `length` units were read above.
        let units = unsafe { std::slice::from_raw_parts(word, length) };
        out.push(String::from_utf16_lossy(units));
    }
    // SAFETY: one block that CommandLineToArgvW allocated.
    unsafe { LocalFree(words.cast()) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_found_by_its_path_or_on_the_path() {
        let folder = tempfile::tempdir().expect("folder");
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(folder.path().join(name), "").expect("write");
        let by_path = folder.path().join("tool");
        assert!(find_program(&by_path.display().to_string()).is_some());
        assert!(find_program(&folder.path().join("none").display().to_string()).is_none());
        assert!(find_program("octotrigger-no-such-program").is_none());
    }

    #[test]
    fn a_tree_is_killed_with_the_processes_it_started() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
            command
        } else {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 60 & wait"]);
            command
        };
        own_tree(&mut command);
        let mut child = command.spawn().expect("spawn");
        let tree = Tree::of(&child).expect("tree");
        assert!(alive(child.id()));
        tree.kill();
        let status = child.wait().expect("wait");
        assert!(!status.success());
        assert!(!alive(child.id()));
    }

    #[cfg(windows)]
    #[test]
    fn a_closed_job_ends_the_agent() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        own_tree(&mut command);
        let mut child = command.spawn().expect("spawn");
        let tree = Tree::of(&child).expect("tree");
        let closed = std::time::Instant::now();
        drop(tree);
        child.wait().expect("wait");
        assert!(
            closed.elapsed() < std::time::Duration::from_secs(30),
            "the job ended the agent when it closed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn words_split_as_windows_programs_read_them() {
        let words =
            split_words(r#"C:\Tools\hermes.exe -z {prompt} -m "claude-sonnet-5-5[1m]" -t a,b"#)
                .expect("words");
        assert_eq!(
            words,
            [
                r"C:\Tools\hermes.exe",
                "-z",
                "{prompt}",
                "-m",
                "claude-sonnet-5-5[1m]",
                "-t",
                "a,b"
            ]
        );
    }
}
