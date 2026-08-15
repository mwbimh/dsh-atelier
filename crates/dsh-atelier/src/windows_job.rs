use std::{ffi::c_void, io};

use tokio::process::Child;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    },
};

/// Owns a Windows Job Object configured to terminate all assigned processes
/// when its last handle closes.
#[derive(Debug)]
pub struct WindowsJob {
    handle: HANDLE,
}

// A Job Object handle is a process-local kernel handle. The Win32 operations
// used by this type are safe to invoke from any thread while the handle lives.
unsafe impl Send for WindowsJob {}
unsafe impl Sync for WindowsJob {}

impl WindowsJob {
    /// Creates an unnamed Job Object with kill-on-close enabled.
    pub fn create() -> io::Result<Self> {
        // SAFETY: null security attributes request the process default ACL and
        // an unnamed object. The returned handle is owned by `WindowsJob`.
        let handle = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
        let job = Self { handle };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact layout and byte size required for the
        // selected information class, and remains alive for the call.
        unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(io::Error::other)?;

        Ok(job)
    }

    /// Assigns a running Tokio child process to this job.
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        let raw_handle = child.raw_handle().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot assign an exited child process to a Job Object",
            )
        })?;
        // SAFETY: Tokio owns the process handle for at least this call. Job
        // assignment does not transfer or close the process handle.
        unsafe { AssignProcessToJobObject(self.handle, HANDLE(raw_handle)) }
            .map_err(io::Error::other)
    }

    /// Terminates every process currently associated with this job.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self` owns a valid Job Object handle for this call.
        unsafe { TerminateJobObject(self.handle, exit_code) }.map_err(io::Error::other)
    }
}

impl Drop for WindowsJob {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` and is closed exactly
        // once here. KILL_ON_JOB_CLOSE intentionally terminates remaining processes.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use std::{io, process::Stdio, time::Duration};

    use tokio::{io::AsyncWriteExt, process::Command, time};
    use windows::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject,
    };

    use super::WindowsJob;

    fn long_running_command() -> Command {
        let mut command =
            Command::new(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
        command
            .args([
                "/d",
                "/s",
                "/c",
                "set /p atelier_gate= & ping 127.0.0.1 -n 30 > nul",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn release_child_gate(child: &mut tokio::process::Child) {
        child
            .stdin
            .as_mut()
            .expect("gated child stdin")
            .write_all(b"go\r\n")
            .await
            .expect("release child gate");
    }

    fn active_processes(job: &WindowsJob) -> u32 {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `accounting` has the layout selected by the information class
        // and `job.handle` remains valid for the duration of the call.
        unsafe {
            QueryInformationJobObject(
                Some(job.handle),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }
        .expect("query Job Object accounting");
        accounting.ActiveProcesses
    }

    async fn wait_for_process_tree(job: &WindowsJob) {
        time::timeout(Duration::from_secs(2), async {
            loop {
                if active_processes(job) >= 2 {
                    return;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cmd.exe should spawn a child process inside the Job Object");
    }

    #[tokio::test]
    async fn terminate_stops_an_assigned_process() {
        let job = WindowsJob::create().expect("create Job Object");
        let mut child = long_running_command().spawn().expect("spawn child");
        job.assign(&child).expect("assign child");
        release_child_gate(&mut child).await;
        wait_for_process_tree(&job).await;

        job.terminate(0xA7E1).expect("terminate Job Object");

        time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("assigned child should terminate promptly")
            .expect("wait for assigned child");
    }

    #[tokio::test]
    async fn dropping_job_stops_an_assigned_process() {
        let job = WindowsJob::create().expect("create Job Object");
        let mut child = long_running_command().spawn().expect("spawn child");
        job.assign(&child).expect("assign child");
        release_child_gate(&mut child).await;
        wait_for_process_tree(&job).await;
        assert!(child.try_wait().expect("query child").is_none());

        drop(job);

        time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("kill-on-close should terminate the child promptly")
            .expect("wait for assigned child");
    }

    #[tokio::test]
    async fn assigning_an_exited_child_is_rejected() {
        let job = WindowsJob::create().expect("create Job Object");
        let mut command =
            Command::new(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
        command.args(["/d", "/s", "/c", "exit /b 0"]);
        let mut child = command.spawn().expect("spawn child");
        child.wait().await.expect("wait for child");

        let error = job.assign(&child).expect_err("exited child has no handle");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
