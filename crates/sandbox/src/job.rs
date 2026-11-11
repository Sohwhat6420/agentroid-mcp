//! The Windows Job Object around the child (`JOB_OBJECT_LIMIT_PROCESS_MEMORY`
//! through `windows-sys`).

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::RawHandle;
use std::ptr::null;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};

/// Exit code given to a terminated job (shows up as the child's exit status).
pub const KILLED_EXIT: u32 = 0xA6E0_0001;

pub struct Job(HANDLE);

/// The limits as the OS reports them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    pub memory_limit: usize,
    pub active_process_limit: u32,
    pub kill_on_close: bool,
    pub peak_process_memory: usize,
}

impl Job {
    /// An anonymous job: `mem` bytes per process, one active process, kill on close, die on an
    /// unhandled exception (no WER dialog), `user_ms` of user-mode CPU per process, no desktop/UI access.
    pub fn new(mem: usize, user_ms: u64) -> io::Result<Job> {
        // SAFETY: plain Win32 calls on a handle we own; every struct is fully initialised and sized.
        unsafe {
            let h = CreateJobObjectW(null(), null());
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(h);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_PROCESS_TIME;
            info.BasicLimitInformation.ActiveProcessLimit = 1;
            info.BasicLimitInformation.PerProcessUserTimeLimit = (user_ms as i64) * 10_000;
            info.ProcessMemoryLimit = mem;
            if SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
            };
            if SetInformationJobObject(
                h,
                JobObjectBasicUIRestrictions,
                &ui as *const _ as *const c_void,
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: `process` is a live process handle owned by the caller's `Child`.
        if unsafe { AssignProcessToJobObject(self.0, process as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `IsProcessInJob(process, this job)`.
    pub fn contains(&self, process: RawHandle) -> bool {
        let mut r = 0;
        // SAFETY: valid handles and an out-pointer to a local.
        let ok = unsafe { IsProcessInJob(process as HANDLE, self.0, &mut r) };
        ok != 0 && r != 0
    }

    pub fn terminate(&self) {
        // SAFETY: our job handle.
        unsafe {
            TerminateJobObject(self.0, KILLED_EXIT);
        }
    }

    /// User + kernel CPU of every process that ran in the job, in ms.
    pub fn cpu_ms(&self) -> u64 {
        let mut a = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: sized out-struct.
        let ok = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                &mut a as *mut _ as *mut c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return 0;
        }
        ((a.TotalUserTime + a.TotalKernelTime).max(0) as u64) / 10_000
    }

    pub fn limits(&self) -> Option<JobLimits> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: sized out-struct.
        let ok = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        let flags = info.BasicLimitInformation.LimitFlags;
        Some(JobLimits {
            memory_limit: if flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0 {
                info.ProcessMemoryLimit
            } else {
                0
            },
            active_process_limit: if flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS != 0 {
                info.BasicLimitInformation.ActiveProcessLimit
            } else {
                0
            },
            kill_on_close: flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
            peak_process_memory: info.PeakProcessMemoryUsed,
        })
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: our handle, closed once. KILL_ON_JOB_CLOSE ends any process still inside.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// `IsProcessInJob(current process, NULL)`: is this process in any job? (The child's self-check.)
pub fn current_process_in_a_job() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut r = 0;
    // SAFETY: pseudo-handle and a local out-pointer.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut r) };
    ok != 0 && r != 0
}
