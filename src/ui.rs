use std::{
    ffi::OsStr,
    io,
    mem::{size_of, zeroed},
    os::windows::io::{FromRawHandle, OwnedHandle},
    path::PathBuf,
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::*,
    Graphics::{
        Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
        Gdi::*,
    },
    System::{
        Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        LibraryLoader::GetModuleHandleW,
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        SystemServices::SS_NOPREFIX,
        Threading::CreateMutexW,
    },
    UI::{
        Controls::{DRAWITEMSTRUCT, EM_SETLIMITTEXT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED},
        HiDpi::*,
        Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus},
        Shell::*,
        WindowsAndMessaging::*,
    },
};

use crate::{
    process::{self, Session},
    profiles::{self, Profile},
    runtime::{self, Installed, Release, Resume},
    startup::{self, State as StartupState},
    telegram::{self, Proxy},
    wide,
};

const START: u16 = 101;
const STOP: u16 = 102;
const TAB_CORE: u16 = 103;
const TAB_PROXY: u16 = 108;
const TAB_UPDATES: u16 = 109;
const CHECK_UPDATE: u16 = 110;
const INSTALL_UPDATE: u16 = 111;
const AUTHOR: u16 = 112;
const ROLLBACK: u16 = 113;
const AUTOSTART: u16 = 114;
const RESUME_COMPONENTS: u16 = 115;
const APP_ICON: u16 = 101;
const SELECT: u16 = 104;
const PROXY_TOGGLE: u16 = 105;
const PROXY_COPY: u16 = 106;
const PORT_EDIT: u16 = 107;
const TIMER: usize = 1;
const TRAY: u32 = WM_APP + 1;
const RESTORE: u32 = WM_APP + 2;
const EXIT: u32 = WM_APP + 3;
const BACKGROUND: u32 = 0x000000;
const SURFACE: u32 = 0x0d0d0d;
const BORDER: u32 = 0x343434;
const ACCENT: u32 = 0x2469ff;
const ACCENT_DIM: u32 = 0x0d1d30;
const TEXT: u32 = 0xefefef;
const DISABLED: u32 = 0x888888;
const CLASS: &str = "ZapretUI.Main";

struct Active {
    session: Session,
    index: usize,
    stopping: Option<Option<usize>>,
}

struct ActiveProxy {
    worker: Proxy,
    starting: Option<Instant>,
    stopping: bool,
}

enum UpdateWork {
    Check(Receiver<Result<runtime::ReleaseCheck, String>>),
    Install(Receiver<Result<Installed, String>>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Core,
    Proxy,
    Updates,
}

struct CoreSwitch {
    target: Installed,
    preferred: String,
    restart: bool,
    rollback: bool,
}

struct Ui {
    window: HWND,
    version_text: HWND,
    core_tab: HWND,
    proxy_tab: HWND,
    updates_tab: HWND,
    startup_button: HWND,
    startup_state: Option<StartupState>,
    port_label: HWND,
    update_status: HWND,
    check_update: HWND,
    install_update: HWND,
    rollback: HWND,
    select: HWND,
    start: HWND,
    stop: HWND,
    status: HWND,
    port: HWND,
    proxy_status: HWND,
    proxy_button: HWND,
    copy: HWND,
    fonts: [HFONT; 2],
    folder: PathBuf,
    installed: Option<runtime::Version>,
    previous_core: Option<Installed>,
    available: Option<Release>,
    update_work: Option<UpdateWork>,
    pending_update: Option<CoreSwitch>,
    tab: Tab,
    profiles: Vec<Profile>,
    selected: usize,
    active: Option<Active>,
    proxy: Option<ActiveProxy>,
    resume: Resume,
    tray: bool,
    taskbar_created: u32,
}

impl Drop for Ui {
    fn drop(&mut self) {
        self.remove_tray();
        unsafe {
            for font in self.fonts {
                DeleteObject(font);
            }
        }
    }
}

struct Layout {
    window: HWND,
    dpi: i32,
    fonts: [HFONT; 2],
}

impl Layout {
    fn control(
        &self,
        class: &str,
        title: &str,
        rect: (i32, i32, i32, i32),
        style: u32,
        id: u16,
        title_font: bool,
    ) -> HWND {
        let (x, y, width, height) = rect;
        unsafe {
            let control = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(title).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                scale(x, self.dpi),
                scale(y, self.dpi),
                scale(width, self.dpi),
                scale(height, self.dpi),
                self.window,
                id as HMENU,
                null_mut(),
                null(),
            );
            SendMessageW(
                control,
                WM_SETFONT,
                self.fonts[usize::from(title_font)] as usize,
                1,
            );
            control
        }
    }

    fn label(&self, title: &str, rect: (i32, i32, i32, i32)) -> HWND {
        self.control("STATIC", title, rect, SS_NOPREFIX, 0, false)
    }

    fn button(&self, title: &str, rect: (i32, i32, i32, i32), id: u16) -> HWND {
        self.control(
            "BUTTON",
            title,
            rect,
            WS_TABSTOP | BS_OWNERDRAW as u32,
            id,
            false,
        )
    }
}

impl Ui {
    fn create(window: HWND, dpi: i32) -> io::Result<Self> {
        let font = |size| unsafe {
            CreateFontW(
                -scale(size, dpi),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                0,
                wide("LLG_Relicus").as_ptr(),
            )
        };
        let layout = Layout {
            window,
            dpi,
            fonts: [font(17), font(33)],
        };
        let (previous, preferred) = profiles::load();
        let bundled = runtime::prepare_core(&previous);
        let heading = layout.control(
            "STATIC",
            "ZAPRET UI -",
            (15, 13, 262, 42),
            SS_NOPREFIX,
            0,
            true,
        );
        let author = layout.control(
            "STATIC",
            "@kelwln",
            (276, 13, 235, 42),
            SS_NOPREFIX,
            AUTHOR,
            true,
        );
        let tray_hint = layout.label(
            "Закрыть окно = свернуть активные процессы в трей.",
            (24, 472, 720, 24),
        );
        let resume = Resume::load();
        let ui = Self {
            window,
            core_tab: layout.button("ZAPRET", (15, 70, 113, 33), TAB_CORE),
            proxy_tab: layout.button("TG WS PROXY", (135, 70, 151, 33), TAB_PROXY),
            updates_tab: layout.button("ОБНОВЛЕНИЯ", (293, 70, 163, 33), TAB_UPDATES),
            startup_button: layout.button("АВТОЗАПУСК", (464, 70, 239, 33), AUTOSTART),
            startup_state: startup::query().ok(),
            select: layout.button("Выбрать стратегию", (15, 117, 293, 34), SELECT),
            start: layout.button("ЗАПУСТИТЬ", (316, 117, 158, 34), START),
            stop: layout.button("ОСТАНОВИТЬ", (482, 117, 157, 34), STOP),
            version_text: layout.label("", (535, 32, 207, 22)),
            status: layout.label("", (24, 202, 704, 245)),
            port_label: layout.label("ПОРТ", (15, 125, 54, 23)),
            port: layout.control(
                "EDIT",
                &resume.proxy_port.unwrap_or(telegram::PORT).to_string(),
                (75, 120, 82, 28),
                WS_TABSTOP | WS_BORDER | ES_CENTER as u32 | ES_NUMBER as u32,
                PORT_EDIT,
                false,
            ),
            proxy_button: layout.button("ЗАПУСТИТЬ ПРОКСИ", (170, 117, 212, 34), PROXY_TOGGLE),
            copy: layout.button("СКОПИРОВАТЬ ССЫЛКУ", (390, 117, 230, 34), PROXY_COPY),
            proxy_status: layout.label("", (24, 202, 704, 245)),
            check_update: layout.button("ПРОВЕРИТЬ", (15, 117, 167, 34), CHECK_UPDATE),
            install_update: layout.button("ОБНОВИТЬ ZAPRET", (190, 117, 228, 34), INSTALL_UPDATE),
            rollback: layout.button("ОТКАТИТЬ ЯДРО", (426, 117, 214, 34), ROLLBACK),
            update_status: layout.label("", (24, 202, 704, 245)),
            fonts: layout.fonts,
            folder: PathBuf::new(),
            installed: None,
            previous_core: None,
            available: None,
            update_work: None,
            pending_update: None,
            tab: Tab::Core,
            profiles: Vec::new(),
            selected: 0,
            active: None,
            proxy: None,
            resume,
            tray: false,
            taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        };
        if [
            heading,
            author,
            tray_hint,
            ui.core_tab,
            ui.proxy_tab,
            ui.updates_tab,
            ui.startup_button,
            ui.select,
            ui.start,
            ui.stop,
            ui.version_text,
            ui.status,
            ui.port_label,
            ui.port,
            ui.proxy_button,
            ui.copy,
            ui.proxy_status,
            ui.check_update,
            ui.install_update,
            ui.rollback,
            ui.update_status,
        ]
        .iter()
        .any(|handle| handle.is_null())
        {
            return Err(io::Error::other("Не удалось создать элементы окна"));
        }
        let panel_label = layout.label("СТАТУС", (24, 173, 500, 20));
        if panel_label.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe { SendMessageW(ui.port, EM_SETLIMITTEXT, 5, 0) };
        let mut ui = ui;
        match bundled {
            Ok(state) => {
                ui.previous_core = state.previous;
                ui.use_core(state.active, &preferred, false);
            }
            Err(error) => set_text(ui.status, format!("Не удалось подготовить Zapret. {error}")),
        }
        set_text(ui.proxy_status, "Остановлен. Адрес подключения: 127.0.0.1.");
        ui.select_tab(Tab::Core);
        ui.show_startup_state();
        Ok(ui)
    }

    fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        for (section, controls) in [
            (
                Tab::Core,
                &[self.select, self.start, self.stop, self.status][..],
            ),
            (
                Tab::Proxy,
                &[
                    self.port_label,
                    self.port,
                    self.proxy_button,
                    self.copy,
                    self.proxy_status,
                ][..],
            ),
            (
                Tab::Updates,
                &[
                    self.check_update,
                    self.install_update,
                    self.rollback,
                    self.update_status,
                ][..],
            ),
        ] {
            for &control in controls {
                unsafe {
                    ShowWindow(control, if section == tab { SW_SHOW } else { SW_HIDE });
                }
            }
        }
        self.buttons();
    }

    fn use_core(&mut self, installed: Installed, preferred: &str, restart: bool) {
        self.installed = Some(installed.version);
        set_text(self.version_text, format!("ZAPRET {}", installed.version));
        self.folder = installed.folder;
        self.reload(preferred);
        if restart {
            if let Err(error) = self.launch(self.selected) {
                set_text(self.status, format!("Не удалось перезапустить. {error}"));
            }
        }
        self.buttons();
    }

    fn check_update(&mut self) -> Result<(), String> {
        self.ensure_timer()?;
        let (send, receive) = mpsc::channel();
        thread::Builder::new()
            .name("zapret-check".into())
            .spawn(move || {
                let _ = send.send(runtime::latest());
            })
            .map_err(|error| error.to_string())?;
        self.update_work = Some(UpdateWork::Check(receive));
        set_text(self.update_status, "Проверка релизов Flowseal...");
        self.buttons();
        Ok(())
    }

    fn install_update(&mut self) -> Result<(), String> {
        let Some(release) = self.available.clone() else {
            return Ok(());
        };
        self.ensure_timer()?;
        let (send, receive) = mpsc::channel();
        let version = release.version;
        thread::Builder::new()
            .name("zapret-update".into())
            .spawn(move || {
                let _ = send.send(runtime::update(&release));
            })
            .map_err(|error| error.to_string())?;
        self.update_work = Some(UpdateWork::Install(receive));
        set_text(self.update_status, format!("Загрузка Zapret {version}..."));
        self.buttons();
        Ok(())
    }

    fn poll_update(&mut self) {
        match self.update_work.take() {
            Some(UpdateWork::Check(receiver)) => {
                let result = match receiver.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => {
                        self.update_work = Some(UpdateWork::Check(receiver));
                        return;
                    }
                    Err(TryRecvError::Disconnected) => Err("Проверка прервалась.".into()),
                };
                match result {
                    Ok(check) => {
                        let version = check.release.version;
                        let source = if check.offline {
                            "Flowseal недоступен. Показана встроенная версия."
                        } else {
                            "Официальные релизы Flowseal."
                        };
                        let current = self
                            .installed
                            .map_or("отсутствует".into(), |v| v.to_string());
                        let prior = self
                            .previous_core
                            .as_ref()
                            .map_or("нет".into(), |v| v.version.to_string());
                        self.available = self
                            .installed
                            .is_none_or(|active| version > active)
                            .then_some(check.release);
                        set_text(self.update_status, format!(
                            "{source}\n\nУстановлено: {current}\nВерсия Flowseal: {version}\nДля отката: {prior}"
                        ));
                    }
                    Err(error) => {
                        self.available = None;
                        set_text(self.update_status, format!("Ошибка проверки. {error}"));
                    }
                }
                self.buttons();
            }
            Some(UpdateWork::Install(receiver)) => match receiver.try_recv() {
                Ok(Ok(core)) => {
                    if let Err(error) = self.switch_core(core, false) {
                        set_text(
                            self.update_status,
                            format!("Переключение ядра не выполнено. {error}"),
                        );
                    }
                    self.buttons();
                }
                Ok(Err(error)) => {
                    set_text(self.update_status, format!("Ошибка обновления. {error}"));
                    self.buttons();
                }
                Err(TryRecvError::Empty) => self.update_work = Some(UpdateWork::Install(receiver)),
                Err(TryRecvError::Disconnected) => {
                    set_text(self.update_status, "Обновление прервалось.");
                    self.buttons();
                }
            },
            None => {}
        }
    }

    fn switch_core(&mut self, target: Installed, rollback: bool) -> Result<(), String> {
        let current = self.active.as_ref();
        let restart = current.is_some_and(|active| active.stopping != Some(None));
        let index = current
            .filter(|active| active.stopping.is_none())
            .map_or(self.selected, |active| active.index);
        let preferred = self
            .profiles
            .get(index)
            .map_or(String::new(), |profile| profile.filename.clone());
        let version = target.version;
        let task = CoreSwitch {
            target,
            preferred,
            restart,
            rollback,
        };
        if self.active.is_some() {
            self.stop(None)?;
            if let Some(active) = &mut self.active {
                active.stopping = Some(None);
            }
            self.pending_update = Some(task);
            set_text(
                self.update_status,
                format!("Подготовлен Zapret {version}. Завершаю текущий запуск..."),
            );
        } else {
            self.finish_switch(task);
        }
        self.buttons();
        Ok(())
    }

    fn finish_switch(&mut self, task: CoreSwitch) {
        let CoreSwitch {
            target,
            preferred,
            restart,
            rollback,
        } = task;
        let version = target.version;
        let previous = self.installed.map(|version| Installed {
            version,
            folder: self.folder.clone(),
        });
        match runtime::activate(&target, previous.as_ref()) {
            Ok(()) => {
                self.use_core(target, &preferred, restart);
                self.previous_core = previous;
                self.available = self
                    .available
                    .take()
                    .filter(|release| release.version > version);
                let message = if rollback {
                    "Выполнен откат до Zapret"
                } else {
                    "Обновлено до Zapret"
                };
                set_text(self.update_status, format!("{message} {version}."));
            }
            Err(error) => {
                set_text(
                    self.update_status,
                    format!("Версия ядра сохранена без изменений. {error}"),
                );
                if restart && !self.profiles.is_empty() {
                    let selected = self
                        .profiles
                        .iter()
                        .position(|profile| profile.filename == preferred)
                        .unwrap_or(self.selected);
                    if let Err(error) = self.launch(selected) {
                        set_text(
                            self.status,
                            format!("Не удалось перезапустить текущую версию. {error}"),
                        );
                    }
                }
            }
        }
        self.buttons();
    }

    fn reload(&mut self, preferred: &str) {
        self.profiles = match profiles::scan(&self.folder) {
            Ok(profiles) => {
                set_text(self.status, "Остановлен");
                profiles
            }
            Err(error) => {
                set_text(self.status, error);
                Vec::new()
            }
        };
        self.selected = self
            .profiles
            .iter()
            .position(|item| item.filename.eq_ignore_ascii_case(preferred))
            .unwrap_or(0);
        self.buttons();
    }

    fn remember(&self) {
        let name = self
            .profiles
            .get(self.selected)
            .map_or("", |item| item.filename.as_str());
        if let Err(error) = profiles::save(&self.folder, name) {
            set_text(
                self.status,
                format!("Не удалось сохранить настройки. {error}"),
            );
        }
    }

    fn show_startup_state(&self) {
        let label = match self.startup_state {
            Some(StartupState::On) => "АВТОЗАПУСК ВКЛ",
            Some(StartupState::Off) => "АВТОЗАПУСК ВЫКЛ",
            Some(StartupState::OtherPath) => "ОБНОВИТЬ АВТОЗАПУСК",
            None => "АВТОЗАПУСК",
        };
        set_text(self.startup_button, label);
        unsafe {
            SetWindowLongPtrW(
                self.startup_button,
                GWLP_USERDATA,
                isize::from(self.startup_state == Some(StartupState::On)),
            );
            InvalidateRect(self.startup_button, null(), 1);
        }
    }

    fn toggle_startup(&mut self) {
        match startup::toggle() {
            Ok(state) => self.startup_state = Some(state),
            Err(error) => {
                self.startup_state = startup::query().ok();
                unsafe {
                    MessageBoxW(
                        self.window,
                        wide(format!("Не удалось изменить автозапуск. {error}")).as_ptr(),
                        wide("Автозапуск Zapret").as_ptr(),
                        MB_ICONERROR,
                    );
                }
            }
        }
        self.show_startup_state();
    }

    fn buttons(&self) {
        let active = self.active.as_ref();
        let stopping = active.is_some_and(|item| item.stopping.is_some());
        let switching = active.is_some_and(|item| item.index != self.selected);
        let installing = matches!(self.update_work, Some(UpdateWork::Install(_)))
            || self.pending_update.is_some();
        let busy = self.update_work.is_some() || self.pending_update.is_some();
        let can_start = !self.profiles.is_empty()
            && !stopping
            && !installing
            && (active.is_none() || switching);
        set_text(
            self.updates_tab,
            if self.available.is_some() {
                "ОБНОВЛЕНИЯ *"
            } else {
                "ОБНОВЛЕНИЯ"
            },
        );
        set_text(
            self.select,
            self.profiles
                .get(self.selected)
                .map_or("Выбрать стратегию".into(), |item| {
                    item.label.clone()
                }),
        );
        set_text(
            self.start,
            if active.is_some() {
                "ПЕРЕКЛЮЧИТЬ"
            } else {
                "ЗАПУСТИТЬ"
            },
        );
        let proxy_stopping = self.proxy.as_ref().is_some_and(|proxy| proxy.stopping);
        set_text(
            self.proxy_button,
            if self.proxy.is_some() {
                "ОСТАНОВИТЬ ПРОКСИ"
            } else if self.resume.proxy_port.is_some() {
                "СБРОСИТЬ ЗАПУСК"
            } else {
                "ЗАПУСТИТЬ ПРОКСИ"
            },
        );
        unsafe {
            EnableWindow(self.start, can_start as i32);
            EnableWindow(
                self.stop,
                (!stopping && (active.is_some() || self.resume.strategy.is_some())) as i32,
            );
            EnableWindow(self.select, (!self.profiles.is_empty() && !stopping) as i32);
            EnableWindow(self.check_update, !busy as i32);
            EnableWindow(
                self.rollback,
                (!busy && self.previous_core.is_some()) as i32,
            );
            EnableWindow(
                self.install_update,
                (!busy && self.available.is_some()) as i32,
            );
            for (button, tab) in [
                (self.core_tab, Tab::Core),
                (self.proxy_tab, Tab::Proxy),
                (self.updates_tab, Tab::Updates),
            ] {
                SetWindowLongPtrW(button, GWLP_USERDATA, isize::from(self.tab == tab));
                InvalidateRect(button, null(), 1);
            }
            EnableWindow(self.port, self.proxy.is_none() as i32);
            EnableWindow(self.proxy_button, !proxy_stopping as i32);
            EnableWindow(
                self.copy,
                self.proxy
                    .as_ref()
                    .is_some_and(|proxy| !proxy.stopping && proxy.starting.is_none())
                    as i32,
            );
        }
    }

    fn select_profile(&mut self) {
        let selected = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (index, profile) in self.profiles.iter().enumerate() {
                let flags = if index == self.selected {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                AppendMenuW(
                    menu,
                    MF_STRING | flags,
                    index + 1,
                    wide(&profile.label).as_ptr(),
                );
            }
            let mut rect = RECT::default();
            GetWindowRect(self.select, &mut rect);
            let item = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                rect.left,
                rect.bottom,
                0,
                self.window,
                null(),
            );
            DestroyMenu(menu);
            item as usize
        };
        if selected != 0 && selected <= self.profiles.len() {
            self.selected = selected - 1;
            self.remember();
            self.buttons();
        }
    }

    fn ensure_timer(&self) -> Result<(), String> {
        if unsafe { SetTimer(self.window, TIMER, 500, None) } == 0 {
            Err("Не удалось создать таймер состояния Windows.".into())
        } else {
            Ok(())
        }
    }

    fn launch(&mut self, index: usize) -> Result<(), String> {
        self.ensure_timer()?;
        if process::winws_running().map_err(|err| err.to_string())? {
            return Err(
                "winws.exe уже запущен. Останови предыдущий запуск или службу Zapret.".into(),
            );
        }
        let profile = &self.profiles[index];
        let args = profiles::launch_args(&self.folder, &profile.filename)?;
        // The official launcher enables timestamps. Keep that setup for profiles
        // which specifically ask winws to use the TCP timestamp option.
        let timestamps = args
            .split_ascii_whitespace()
            .filter_map(|arg| arg.strip_prefix("--dpi-desync-fooling="))
            .any(|methods| methods.split(',').any(|method| method == "ts"));
        let timestamp_warning = timestamps && process::enable_tcp_timestamps().is_err();
        let bin = self.folder.join("bin");
        let session = Session::start(&bin.join("winws.exe"), &bin, args.as_ref())
            .map_err(|err| err.to_string())?;
        self.active = Some(Active {
            session,
            index,
            stopping: None,
        });
        let warning = if timestamp_warning {
            "\nНе удалось включить TCP timestamps."
        } else {
            ""
        };
        set_text(
            self.status,
            format!("Работает / {}{warning}", profile.label),
        );
        self.buttons();
        Ok(())
    }

    fn stop(&mut self, next: Option<usize>) -> Result<(), String> {
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.stopping.is_none())
        {
            active.session.stop().map_err(|err| err.to_string())?;
            active.stopping = Some(next);
            set_text(
                self.status,
                if next.is_some() {
                    "Переключение..."
                } else {
                    "Остановка..."
                },
            );
            self.buttons();
        }
        Ok(())
    }

    fn launch_proxy(&mut self, port: u16) -> Result<(), String> {
        self.ensure_timer()?;
        let worker = Proxy::start(port)?;
        self.proxy = Some(ActiveProxy {
            worker,
            starting: Some(Instant::now()),
            stopping: false,
        });
        set_text(self.proxy_status, "Запуск TG WS Proxy...");
        self.buttons();
        Ok(())
    }

    fn save_resume(&mut self, next: Resume) -> Result<(), String> {
        next.save()
            .map_err(|error| format!("Не удалось сохранить запуск. {error}"))?;
        self.resume = next;
        self.buttons();
        Ok(())
    }

    fn resume_components(&mut self) {
        if let Some(filename) = &self.resume.strategy {
            let result = self
                .profiles
                .iter()
                .position(|profile| profile.filename.eq_ignore_ascii_case(filename))
                .or_else(|| (!self.profiles.is_empty()).then_some(self.selected))
                .ok_or_else(|| "Нет доступных стратегий.".to_owned());
            match result {
                Ok(index) if self.active.is_none() => {
                    self.selected = index;
                    if let Err(error) = self.launch(index) {
                        set_text(
                            self.status,
                            format!("Не удалось восстановить Zapret. {error}"),
                        );
                    }
                }
                Err(error) => set_text(
                    self.status,
                    format!("Не удалось восстановить Zapret. {error}"),
                ),
                _ => {}
            }
        }
        if let Some(port) = self.resume.proxy_port.filter(|&port| port > 0) {
            if self.proxy.is_none() {
                if let Err(error) = self.launch_proxy(port) {
                    set_text(
                        self.proxy_status,
                        format!("Не удалось восстановить TG WS Proxy. {error}"),
                    );
                }
            }
        }
        self.buttons();
    }

    fn start_or_switch(&mut self) -> Result<(), String> {
        let next = self.selected;
        match self.active.as_ref() {
            None => self.launch(next)?,
            Some(active) if active.stopping.is_none() && active.index != next => {
                self.stop(Some(next))?
            }
            Some(_) => return Ok(()),
        }
        self.save_resume(Resume {
            strategy: Some(self.profiles[next].filename.clone()),
            ..self.resume.clone()
        })
    }

    fn stop_zapret(&mut self) -> Result<(), String> {
        self.stop(None)?;
        self.save_resume(Resume {
            strategy: None,
            ..self.resume.clone()
        })?;
        if self.active.is_none() && !self.profiles.is_empty() {
            set_text(self.status, "Остановлен");
        }
        Ok(())
    }

    fn toggle_proxy(&mut self) -> Result<(), String> {
        if let Some(proxy) = &mut self.proxy {
            if proxy.stopping {
                return Ok(());
            }
            proxy.worker.session.stop().map_err(|err| err.to_string())?;
            proxy.stopping = true;
            set_text(self.proxy_status, "Остановка...");
            self.buttons();
        }
        let port = if self.proxy.is_some() || self.resume.proxy_port.is_some() {
            if self.proxy.is_none() {
                set_text(
                    self.proxy_status,
                    "Остановлен. Адрес подключения: 127.0.0.1.",
                );
            }
            None
        } else {
            let port = window_text(self.port)
                .parse::<u16>()
                .ok()
                .filter(|&port| port > 0)
                .ok_or("Введи порт от 1 до 65535.")?;
            self.launch_proxy(port)?;
            Some(port)
        };
        self.save_resume(Resume {
            proxy_port: port,
            ..self.resume.clone()
        })
    }

    fn tick(&mut self) {
        if let Some(active) = &self.active {
            match active.session.exit_code() {
                Ok(None) => {}
                result => {
                    let pending = active.stopping;
                    self.active = None;
                    match (pending, result) {
                        (Some(Some(index)), Ok(Some(_))) => {
                            if let Err(err) = self.launch(index) {
                                set_text(
                                    self.status,
                                    format!("Не удалось переключить стратегию. {err}"),
                                );
                            }
                        }
                        (Some(None), Ok(Some(_))) => set_text(self.status, "Остановлен"),
                        (_, Ok(Some(code))) => {
                            set_text(self.status, format!("winws.exe завершился с кодом {code}."))
                        }
                        (_, Err(err)) => {
                            set_text(self.status, format!("Ошибка проверки состояния. {err}"))
                        }
                        (_, Ok(None)) => unreachable!(),
                    }
                    self.buttons();
                }
            }
        }
        if let Some(proxy) = &mut self.proxy {
            let finished = match proxy.worker.session.exit_code() {
                Ok(Some(_)) if proxy.stopping => Some("Остановлен.".into()),
                Ok(Some(code)) => Some(format!("TG WS Proxy завершился с кодом {code}.")),
                Err(err) => Some(format!("Ошибка проверки прокси. {err}")),
                Ok(None) => {
                    if let Some(since) = proxy.starting.filter(|_| !proxy.stopping) {
                        if proxy.worker.listening() {
                            proxy.starting = None;
                            set_text(
                                self.proxy_status,
                                format!("Работает / 127.0.0.1:{}", proxy.worker.port),
                            );
                            self.buttons();
                        } else if since.elapsed() >= Duration::from_secs(12) {
                            self.proxy = None;
                            set_text(self.proxy_status, "Прокси не открыл порт за 12 секунд.");
                            self.buttons();
                        }
                    }
                    None
                }
            };
            if let Some(message) = finished {
                self.proxy = None;
                set_text(self.proxy_status, message);
                self.buttons();
            }
        }
        if self.active.is_none() {
            if let Some(task) = self.pending_update.take() {
                self.finish_switch(task);
            }
        }
        self.poll_update();
        self.stop_idle_timer();
    }

    fn stop_idle_timer(&self) {
        if self.active.is_none()
            && self.proxy.is_none()
            && self.update_work.is_none()
            && self.pending_update.is_none()
        {
            unsafe { KillTimer(self.window, TIMER) };
        }
    }

    fn command(&mut self, id: u16) {
        let result = match id {
            AUTOSTART => {
                self.toggle_startup();
                Ok(())
            }
            TAB_CORE => {
                self.select_tab(Tab::Core);
                Ok(())
            }
            TAB_PROXY => {
                self.select_tab(Tab::Proxy);
                Ok(())
            }
            TAB_UPDATES => {
                self.select_tab(Tab::Updates);
                Ok(())
            }
            CHECK_UPDATE if self.update_work.is_none() && self.pending_update.is_none() => {
                self.check_update()
            }
            INSTALL_UPDATE if self.update_work.is_none() && self.pending_update.is_none() => {
                self.install_update()
            }
            ROLLBACK if self.update_work.is_none() && self.pending_update.is_none() => {
                if let Some(previous) = self.previous_core.clone() {
                    self.switch_core(previous, true)
                } else {
                    Ok(())
                }
            }
            SELECT => {
                self.select_profile();
                Ok(())
            }
            RESUME_COMPONENTS => {
                self.resume_components();
                Ok(())
            }
            START if self.selected < self.profiles.len() && self.pending_update.is_none() => {
                self.start_or_switch()
            }
            STOP => self.stop_zapret(),
            PROXY_TOGGLE => self.toggle_proxy(),
            PROXY_COPY => {
                if let Some(proxy) = &self.proxy {
                    let status = copy_to_clipboard(self.window, &proxy.worker.link());
                    if status.is_ok() {
                        set_text(
                            self.proxy_status,
                            "Ссылка скопирована. Открой её, чтобы добавить прокси.",
                        );
                    }
                    status
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            set_text(
                if matches!(id, PROXY_TOGGLE | PROXY_COPY) {
                    self.proxy_status
                } else if matches!(id, CHECK_UPDATE | INSTALL_UPDATE | ROLLBACK) {
                    self.update_status
                } else {
                    self.status
                },
                err,
            );
        }
        self.stop_idle_timer();
    }

    fn add_tray(&mut self) -> bool {
        let mut data = self.tray_data();
        data.uFlags = NIF_MESSAGE | NIF_TIP | NIF_ICON;
        data.uCallbackMessage = TRAY;
        data.hIcon = unsafe { application_icon(true) };
        let tip = wide("Zapret и TG WS Proxy");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        self.tray = unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 };
        self.tray
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: 1,
            ..Default::default()
        }
    }

    fn remove_tray(&mut self) {
        if self.tray {
            unsafe { Shell_NotifyIconW(NIM_DELETE, &self.tray_data()) };
            self.tray = false;
        }
    }

    fn restore(&mut self) {
        self.remove_tray();
        unsafe {
            ShowWindow(self.window, SW_RESTORE);
            SetForegroundWindow(self.window);
        }
    }

    fn tray_menu(&mut self) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING, 1, wide("Открыть Zapret").as_ptr());
            AppendMenuW(menu, MF_STRING, 2, wide("Остановить всё и выйти").as_ptr());
            let mut mouse = POINT::default();
            GetCursorPos(&mut mouse);
            SetForegroundWindow(self.window);
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                mouse.x,
                mouse.y,
                0,
                self.window,
                null(),
            );
            PostMessageW(self.window, WM_NULL, 0, 0);
            DestroyMenu(menu);
            match choice {
                1 => self.restore(),
                2 => {
                    PostMessageW(self.window, EXIT, 0, 0);
                }
                _ => {}
            }
        }
    }
}

fn scale(value: i32, dpi: i32) -> i32 {
    (value * dpi + 48) / 96
}

fn set_text(window: HWND, value: impl AsRef<OsStr>) {
    unsafe { SetWindowTextW(window, wide(value).as_ptr()) };
}

fn window_text(window: HWND) -> String {
    unsafe {
        let mut buffer = vec![0; GetWindowTextLengthW(window).max(0) as usize + 1];
        let length = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32).max(0);
        String::from_utf16_lossy(&buffer[..length as usize])
    }
}

fn copy_to_clipboard(window: HWND, text: &str) -> Result<(), String> {
    unsafe {
        let text = wide(text);
        let bytes = text.len() * size_of::<u16>();
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            return Err("Не удалось выделить память буфера обмена.".into());
        }
        let buffer = GlobalLock(memory).cast::<u16>();
        let mut copied = false;
        if !buffer.is_null() {
            std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
            GlobalUnlock(memory);
            if OpenClipboard(window) != 0 {
                // CF_UNICODETEXT transfers ownership of the allocation on success.
                copied = EmptyClipboard() != 0 && !SetClipboardData(13, memory).is_null();
                CloseClipboard();
            }
        }
        if copied {
            return Ok(());
        }
        GlobalFree(memory);
        Err("Не удалось скопировать ссылку в буфер обмена.".into())
    }
}

unsafe fn draw_button(item: &DRAWITEMSTRUCT) {
    let disabled = item.itemState & ODS_DISABLED != 0;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let primary = matches!(item.CtlID as u16, START | PROXY_TOGGLE | INSTALL_UPDATE)
        || GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) != 0;
    let fill = if disabled {
        BACKGROUND
    } else if pressed || primary {
        ACCENT_DIM
    } else {
        BACKGROUND
    };
    let text_color = if disabled {
        DISABLED
    } else if primary {
        ACCENT
    } else {
        TEXT
    };
    let brush = GetStockObject(DC_BRUSH);
    SetDCBrushColor(item.hDC, fill);
    FillRect(item.hDC, &item.rcItem, brush);
    SetDCBrushColor(
        item.hDC,
        if disabled {
            BORDER
        } else if item.itemState & ODS_FOCUS != 0 {
            TEXT
        } else {
            ACCENT
        },
    );
    FrameRect(item.hDC, &item.rcItem, brush);
    SetBkMode(item.hDC, TRANSPARENT as i32);
    SetTextColor(item.hDC, text_color);
    let mut rect = item.rcItem;
    let align = if item.CtlID == SELECT as u32 {
        rect.left += scale(12, GetDpiForWindow(item.hwndItem) as i32);
        DT_LEFT
    } else {
        DT_CENTER
    };
    let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
    let old_font = if font.is_null() {
        null_mut()
    } else {
        SelectObject(item.hDC, font)
    };
    let value = wide(window_text(item.hwndItem));
    DrawTextW(
        item.hDC,
        value.as_ptr(),
        -1,
        &mut rect,
        align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    if item.CtlID == SELECT as u32 {
        let mut arrow = item.rcItem;
        arrow.right -= scale(13, GetDpiForWindow(item.hwndItem) as i32);
        DrawTextW(
            item.hDC,
            wide("v").as_ptr(),
            -1,
            &mut arrow,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }
    if !old_font.is_null() {
        SelectObject(item.hDC, old_font);
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            let mut client = RECT::default();
            GetClientRect(window, &mut client);
            let dpi = GetDpiForWindow(window).max(96) as i32;
            let brush = GetStockObject(DC_BRUSH);
            SetDCBrushColor(dc, BORDER);
            let rect = RECT {
                left: scale(15, dpi),
                right: client.right - scale(15, dpi),
                top: scale(164, dpi),
                bottom: scale(460, dpi),
            };
            FrameRect(dc, &rect, brush);
            EndPaint(window, &paint);
            return 0;
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
            let bg = if message == WM_CTLCOLOREDIT {
                SURFACE
            } else {
                BACKGROUND
            };
            SetTextColor(
                dc,
                if GetDlgCtrlID(lparam as HWND) == i32::from(AUTHOR) {
                    ACCENT
                } else {
                    TEXT
                },
            );
            SetBkColor(dc, bg);
            SetDCBrushColor(dc, bg);
            return GetStockObject(DC_BRUSH) as LRESULT;
        }
        WM_DRAWITEM if lparam != 0 => {
            draw_button(&*(lparam as *const DRAWITEMSTRUCT));
            return 1;
        }
        WM_DESTROY => {
            KillTimer(window, TIMER);
            PostQuitMessage(0);
            return 0;
        }
        WM_NCDESTROY => {
            let state = SetWindowLongPtrW(window, GWLP_USERDATA, 0) as *mut Ui;
            if !state.is_null() {
                drop(Box::from_raw(state));
            }
        }
        _ => {
            let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Ui;
            if !state.is_null() {
                let taskbar_created = (*state).taskbar_created;
                if matches!(
                    message,
                    WM_COMMAND | WM_TIMER | WM_CLOSE | TRAY | RESTORE | EXIT
                ) || (taskbar_created != 0 && message == taskbar_created)
                {
                    // Opening a native popup can dispatch nested window messages.
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                    let ui = &mut *state;
                    let mut exit = false;
                    match message {
                        WM_COMMAND if (wparam >> 16) as u16 == BN_CLICKED as u16 => {
                            ui.command(wparam as u16)
                        }
                        WM_TIMER if wparam == TIMER => ui.tick(),
                        WM_CLOSE => {
                            if ui.active.is_none()
                                && ui.proxy.is_none()
                                && !matches!(ui.update_work, Some(UpdateWork::Install(_)))
                            {
                                exit = true;
                            } else if ui.add_tray() {
                                ShowWindow(window, SW_HIDE);
                            } else {
                                ShowWindow(window, SW_MINIMIZE);
                            }
                        }
                        TRAY => match lparam as u32 {
                            WM_LBUTTONUP => ui.restore(),
                            WM_RBUTTONUP => ui.tray_menu(),
                            _ => {}
                        },
                        RESTORE => ui.restore(),
                        EXIT => exit = true,
                        msg if msg == taskbar_created && ui.tray => {
                            if !ui.add_tray() {
                                ui.restore();
                            }
                        }
                        _ => {}
                    }
                    SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);
                    if exit {
                        DestroyWindow(window);
                    }
                    return 0;
                }
            } else if message == WM_CLOSE {
                return 0;
            }
        }
    }
    DefWindowProcW(window, message, wparam, lparam)
}

fn show_message(message: &str, flags: u32) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("Zapret").as_ptr(),
            flags,
        )
    };
}

unsafe fn application_icon(small: bool) -> HICON {
    let (width, height) = if small {
        (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
    } else {
        (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
    };
    let icon = LoadImageW(
        GetModuleHandleW(null()),
        APP_ICON as usize as *const u16,
        IMAGE_ICON,
        width,
        height,
        LR_SHARED,
    );
    if icon.is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        icon
    }
}

unsafe fn open_window(start_in_tray: bool) -> io::Result<()> {
    let instance = GetModuleHandleW(null());
    let class = wide(CLASS);
    let background = CreateSolidBrush(BACKGROUND);
    let window_class = WNDCLASSW {
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        lpfnWndProc: Some(window_proc),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hIcon: application_icon(false),
        hbrBackground: background,
        ..Default::default()
    };
    if RegisterClassW(&window_class) == 0 {
        DeleteObject(background);
        return Err(io::Error::last_os_error());
    }
    let result = message_loop(instance, &class, start_in_tray);
    UnregisterClassW(class.as_ptr(), instance);
    DeleteObject(background);
    result
}

unsafe fn message_loop(instance: HINSTANCE, class: &[u16], start_in_tray: bool) -> io::Result<()> {
    let dpi = GetDpiForSystem().max(96) as i32;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: scale(756, dpi),
        bottom: scale(506, dpi),
    };
    let ex_style = WS_EX_CONTROLPARENT;
    if AdjustWindowRectExForDpi(&mut rect, style, 0, ex_style, dpi as u32) == 0 {
        return Err(io::Error::last_os_error());
    }
    let window = CreateWindowExW(
        ex_style,
        class.as_ptr(),
        wide("Zapret").as_ptr(),
        style,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        rect.right - rect.left,
        rect.bottom - rect.top,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if window.is_null() {
        return Err(io::Error::last_os_error());
    }

    let enabled: i32 = 1;
    DwmSetWindowAttribute(
        window,
        DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
        (&enabled as *const i32).cast(),
        size_of::<i32>() as u32,
    );
    SendMessageW(
        window,
        WM_SETICON,
        ICON_SMALL as usize,
        application_icon(true) as isize,
    );
    let mut ui = match Ui::create(window, dpi) {
        Ok(ui) => Box::new(ui),
        Err(error) => {
            DestroyWindow(window);
            return Err(error);
        }
    };
    let initial_focus = ui.core_tab;
    let tray_added = start_in_tray && ui.add_tray();
    SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(ui) as isize);
    if !tray_added {
        if start_in_tray {
            ShowWindow(window, SW_SHOWMINNOACTIVE);
        } else {
            ShowWindow(window, SW_SHOW);
            SetFocus(initial_focus);
        }
    }
    PostMessageW(window, WM_COMMAND, RESUME_COMPONENTS as usize, 0);
    PostMessageW(window, WM_COMMAND, CHECK_UPDATE as usize, 0);

    let mut msg: MSG = zeroed();
    loop {
        match GetMessageW(&mut msg, null_mut(), 0, 0) {
            0 => return Ok(()),
            -1 => {
                let error = io::Error::last_os_error();
                DestroyWindow(window);
                return Err(error);
            }
            _ if msg.message == WM_KEYDOWN
                && msg.wParam == 13
                && GetDlgCtrlID(GetFocus()) != i32::from(PORT_EDIT) =>
            {
                let focused = GetFocus();
                if !focused.is_null() {
                    SendMessageW(focused, BM_CLICK, 0, 0);
                }
            }
            _ if IsDialogMessageW(window, &msg) != 0 => {}
            _ => {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

pub fn run() {
    let start_in_tray = std::env::args_os()
        .skip(1)
        .any(|arg| arg == startup::ARGUMENT);
    unsafe {
        let mutex = CreateMutexW(null(), 0, wide(r"Local\ZapretUI").as_ptr());
        let status = GetLastError();
        if mutex.is_null() {
            show_message(&format!("Ошибка запуска Windows ({status})."), MB_ICONERROR);
            return;
        }
        let _single_instance = OwnedHandle::from_raw_handle(mutex);
        if status == ERROR_ALREADY_EXISTS {
            if start_in_tray {
                return;
            }
            let current = FindWindowW(wide(CLASS).as_ptr(), null());
            if current.is_null() {
                show_message("Окно Zapret уже открывается.", MB_ICONINFORMATION);
            } else {
                PostMessageW(current, RESTORE, 0, 0);
            }
            return;
        }
        let init = CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        if init < 0 {
            show_message(
                &format!(
                    "Не удалось запустить интерфейс Windows (0x{:08X}).",
                    init as u32
                ),
                MB_ICONERROR,
            );
            return;
        }
        let mut font = include_bytes!("../assets/LLG_Relicus.ttf").to_vec();
        let mut count = 0;
        let resource = AddFontMemResourceEx(
            font.as_mut_ptr().cast(),
            font.len() as u32,
            null_mut(),
            &mut count,
        );
        let result = open_window(start_in_tray);
        if !resource.is_null() {
            RemoveFontMemResourceEx(resource);
        }
        CoUninitialize();
        if let Err(error) = result {
            show_message(&format!("Не удалось открыть окно. {error}"), MB_ICONERROR);
        }
    }
}
