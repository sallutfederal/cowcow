// A Windows Job Object, so a timed-out command takes its grandchildren with it.
//
// Killing `cmd /C start something` with `Child::kill` only kills `cmd`. The
// children it spawned keep running, and a model that timed out on `npm install`
// would leave the install going. A Job Object is the OS answer: every process
// assigned to it is killed when the last handle to it closes, so dropping this
// handle is the kill switch for the whole tree.
//
// Verified against windows 0.61.3 (Win32_System_JobObjects).

#![cfg(windows)]

use std::io;
use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// Owns one job. Closing the handle kills everything still in it.
pub struct JobHandle(HANDLE);

// A job handle is a bare kernel handle: it can be moved between threads and
// used from several, as long as it is not closed twice. Drop does the closing,
// and Drop is the only thing that closes it.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl JobHandle {
    /// Creates a job whose members die with it.
    pub fn new() -> io::Result<Self> {
        unsafe {
            let job = CreateJobObjectW(None, None).map_err(|e| {
                io::Error::other(format!("CreateJobObjectW: {e}"))
            })?;

            // Zeroed, then one flag: kill-on-close. Nothing else is configured,
            // so the job imposes no limit of its own.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| {
                let _ = CloseHandle(job);
                io::Error::other(format!("SetInformationJobObject: {e}"))
            })?;

            Ok(JobHandle(job))
        }
    }

    /// Puts an existing process into the job.
    ///
    /// Fails with ERROR_ACCESS_DENIED when the process is already in another
    /// job, which happens if the app itself runs inside one. The caller logs and
    /// carries on: the timeout still works, only the guarantee about
    /// grandchildren weakens.
    pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
        unsafe {
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid).map_err(
                |e| io::Error::other(format!("OpenProcess({pid}): {e}")),
            )?;

            let result = AssignProcessToJobObject(self.0, process).map_err(|e| {
                io::Error::other(format!("AssignProcessToJobObject: {e}"))
            });
            let _ = CloseHandle(process);
            result
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // The point of the whole type: this is what kills the tree.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
