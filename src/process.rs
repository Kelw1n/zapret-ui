use std::{
    ffi::OsStr,
    io,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
    ptr::null,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Diagnostics::ToolHelp::*, JobObjects::*, SystemInformation::GetSystemDirectoryW,
        Threading::*,
    },
};

use crate::wide;

fn checked(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn handle(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}

pub struct Session {
    job: OwnedHandle,
    process: OwnedHandle,
}

impl Session {
    pub fn start(exe: &Path, folder: &Path, arguments: &OsStr) -> io::Result<Self> {
        unsafe {
            let job = handle(CreateJobObjectW(null(), null()))?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            checked(SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            ))?;

            let executable = wide(exe.as_os_str());
            let mut command = std::ffi::OsString::from("\"");
            command.push(exe.as_os_str());
            command.push("\" ");
            command.push(arguments);
            let mut command = wide(command);
            let directory = wide(folder.as_os_str());
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                ..Default::default()
            };
            let mut child = PROCESS_INFORMATION::default();
            checked(CreateProcessW(
                executable.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                null(),
                directory.as_ptr(),
                &startup,
                &mut child,
            ))?;

            let process = OwnedHandle::from_raw_handle(child.hProcess);
            let thread = OwnedHandle::from_raw_handle(child.hThread);
            // Assign the child before it can create any descendants.
            if let Err(error) = checked(AssignProcessToJobObject(
                job.as_raw_handle(),
                process.as_raw_handle(),
            )) {
                TerminateProcess(process.as_raw_handle(), 1);
                return Err(error);
            }
            if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { job, process })
        }
    }

    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        unsafe {
            match WaitForSingleObject(self.process.as_raw_handle(), 0) {
                WAIT_TIMEOUT => Ok(None),
                WAIT_OBJECT_0 => {
                    let mut code = 0;
                    checked(GetExitCodeProcess(self.process.as_raw_handle(), &mut code))?;
                    Ok(Some(code))
                }
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    pub fn stop(&self) -> io::Result<()> {
        checked(unsafe { TerminateJobObject(self.job.as_raw_handle(), 0) })
    }
}

pub fn winws_running() -> io::Result<bool> {
    let snapshot = handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    checked(unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) })?;
    let name = wide("winws.exe");
    loop {
        let matches = name.iter().zip(entry.szExeFile).all(|(&expected, actual)| {
            expected == actual || ((65..=90).contains(&actual) && expected == actual + 32)
        });
        if matches {
            return Ok(true);
        }
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES as i32)
            {
                return Ok(false);
            }
            return Err(err);
        }
    }
}

pub fn enable_tcp_timestamps() -> io::Result<()> {
    let mut system = [0; 32768];
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    if length == 0 || length >= system.len() {
        return Err(io::Error::last_os_error());
    }
    let system = std::path::PathBuf::from(std::ffi::OsString::from_wide(&system[..length]));
    let status = Command::new(system.join("netsh.exe"))
        .args(["interface", "tcp", "set", "global", "timestamps=enabled"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("netsh: {status}")))
    }
}
