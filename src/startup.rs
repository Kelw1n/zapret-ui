use std::{
    env, fs, io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};

use windows::{
    core::{Interface, BSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, VARIANT_FALSE},
        System::{
            Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
            TaskScheduler::{
                IExecAction, ILogonTrigger, ITaskFolder, ITaskService, TaskScheduler,
                TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE, TASK_INSTANCES_IGNORE_NEW,
                TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_HIGHEST, TASK_TRIGGER_LOGON,
            },
            Variant::VARIANT,
        },
    },
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SetFileSecurityW, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, MoveFileExW, FILE_ATTRIBUTE_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
    },
    System::{
        Com::CoTaskMemFree,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
    UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath},
};

use crate::wide;

pub const ARGUMENT: &str = "--autostart";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Off,
    On,
    OtherPath,
}

struct Scheduler {
    service: ITaskService,
    folder: ITaskFolder,
    name: BSTR,
    sid: String,
}

impl Scheduler {
    fn connect() -> Result<Self, String> {
        let sid = current_user_sid().map_err(|error| error.to_string())?;
        let empty = VARIANT::default();
        // The UI thread initializes COM before querying Task Scheduler.
        unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
                    .map_err(|error| error.to_string())?;
            service
                .Connect(&empty, &empty, &empty, &empty)
                .map_err(|error| error.to_string())?;
            let folder = service
                .GetFolder(&BSTR::from("\\"))
                .map_err(|error| error.to_string())?;
            let name = BSTR::from(format!("ZapretUI-{sid}"));
            Ok(Self {
                service,
                folder,
                name,
                sid,
            })
        }
    }

    fn state(&self, executable: &Path) -> windows::core::Result<State> {
        unsafe {
            let task = match self.folder.GetTask(&self.name) {
                Ok(task) => task,
                Err(error)
                    if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                        || error.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
                {
                    return Ok(State::Off)
                }
                Err(error) => return Err(error),
            };
            if task.Enabled()? == VARIANT_FALSE {
                return Ok(State::Off);
            }
            let action: IExecAction = task.Definition()?.Actions()?.get_Item(1)?.cast()?;
            let mut registered = BSTR::new();
            action.Path(&mut registered)?;
            Ok(
                if registered
                    .iter()
                    .copied()
                    .eq(executable.as_os_str().encode_wide())
                    && executable.is_file()
                {
                    State::On
                } else {
                    State::OtherPath
                },
            )
        }
    }

    fn set(&self, state: State, executable: &Path) -> windows::core::Result<State> {
        unsafe {
            if state == State::On {
                self.folder.DeleteTask(&self.name, 0)?;
                return Ok(State::Off);
            }

            let definition = self.service.NewTask(0)?;
            let sid = BSTR::from(self.sid.as_str());
            let principal = definition.Principal()?;
            principal.SetUserId(&sid)?;
            principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
            principal.SetRunLevel(TASK_RUNLEVEL_HIGHEST)?;

            let trigger: ILogonTrigger =
                definition.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
            trigger.SetUserId(&sid)?;

            let action: IExecAction = definition.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
            let path = executable.as_os_str().encode_wide().collect::<Vec<_>>();
            action.SetPath(&BSTR::from_wide(&path))?;
            action.SetArguments(&BSTR::from(ARGUMENT))?;

            let settings = definition.Settings()?;
            settings.SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)?;
            settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
            settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
            settings.SetExecutionTimeLimit(&BSTR::from("PT0S"))?;

            let empty = VARIANT::default();
            self.folder.RegisterTaskDefinition(
                &self.name,
                &definition,
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::from(self.sid.as_str()),
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )?;
            Ok(State::On)
        }
    }
}

pub fn query() -> Result<State, String> {
    let executable = protected_executable().map_err(|error| error.to_string())?;
    Scheduler::connect()?
        .state(&executable)
        .map_err(|error| error.to_string())
}

pub fn toggle() -> Result<State, String> {
    let executable = protected_executable().map_err(|error| error.to_string())?;
    let scheduler = Scheduler::connect()?;
    let state = scheduler
        .state(&executable)
        .map_err(|error| error.to_string())?;
    if state != State::On {
        install_copy(&executable).map_err(|error| error.to_string())?;
    }
    scheduler
        .set(state, &executable)
        .map_err(|error| error.to_string())
}

fn protected_executable() -> io::Result<PathBuf> {
    unsafe {
        let mut folder = null_mut();
        let result = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, null_mut(), &mut folder);
        if result < 0 || folder.is_null() {
            return Err(io::Error::other(format!(
                "Program Files (0x{:08X})",
                result as u32
            )));
        }
        let directory = PathBuf::from(std::ffi::OsString::from_wide(wide_slice(folder)));
        CoTaskMemFree(folder.cast());
        Ok(directory.join("ZapretUI-Autostart").join(concat!(
            "ZapretUI-",
            env!("CARGO_PKG_VERSION"),
            ".exe"
        )))
    }
}

fn install_copy(destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("Каталог автозапуска недоступен."))?;
    protect_directory(parent)?;
    let source = env::current_exe()?;
    match fs::symlink_metadata(destination) {
        Ok(metadata)
            if !metadata.is_file()
                || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 =>
        {
            return Err(io::Error::other("Некорректный файл автозапуска."));
        }
        Ok(_) if source == destination => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let staging = destination.with_extension(format!("tmp-{}", std::process::id()));
    let mut output = fs::File::create_new(&staging)?;
    let result = (|| {
        io::copy(&mut fs::File::open(source)?, &mut output)?;
        output.sync_all()?;
        drop(output);
        let moved = unsafe {
            MoveFileExW(
                wide(staging.as_os_str()).as_ptr(),
                wide(destination.as_os_str()).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    result
}

fn protect_directory(path: &Path) -> io::Result<()> {
    // The higher-privilege logon task must never target a user-writable executable.
    let rules = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)");
    let mut descriptor = null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            rules.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let path_wide = wide(path.as_os_str());
    let result = (|| {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("Некорректный каталог автозапуска."));
        }
        if unsafe {
            SetFileSecurityW(
                path_wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    unsafe { LocalFree(descriptor) };
    result
}

unsafe fn wide_slice<'a>(text: *const u16) -> &'a [u16] {
    let mut length = 0;
    while *text.add(length) != 0 {
        length += 1;
    }
    std::slice::from_raw_parts(text, length)
}

fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut length = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length);
        if length < size_of::<TOKEN_USER>() as u32 {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            length,
            &mut length,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*data.as_ptr().cast::<TOKEN_USER>();
        let mut text = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = String::from_utf16_lossy(wide_slice(text));
        LocalFree(text.cast());
        Ok(sid)
    }
}
