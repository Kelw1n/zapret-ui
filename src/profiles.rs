use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

pub const DEFAULT_FOLDER: &str = r"D:\zapret-discord-youtube-1.10.2";

pub struct Profile {
    pub filename: String,
    pub label: String,
}

fn profile(filename: String) -> Option<Profile> {
    // Match the strategy names shipped in this release.
    if !filename.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'(' | b')' | b'_' | b'-')
    }) {
        return None;
    }
    let lowercase = filename.to_ascii_lowercase();
    let stem = lowercase.strip_suffix(".bat")?;
    let label = if stem == "general" {
        "General".to_owned()
    } else {
        let suffix = stem.strip_prefix("general (")?.strip_suffix(')')?;
        if suffix.trim().is_empty() {
            return None;
        }
        filename[9..filename.len() - 5].to_owned()
    };
    Some(Profile { filename, label })
}

pub fn scan(folder: &Path) -> Result<Vec<Profile>, String> {
    let entries = fs::read_dir(folder).map_err(|err| format!("Папка недоступна ({err})."))?;
    if !folder.join("bin").join("winws.exe").is_file() || !folder.join("service.bat").is_file() {
        return Err("В папке должны быть service.bat и bin\\winws.exe.".into());
    }
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("Не удалось прочитать папку ({err})."))?;
        if entry.path().is_file() {
            if let Some(profile) = entry.file_name().into_string().ok().and_then(profile) {
                profiles.push(profile);
            }
        }
    }
    profiles.sort_by_cached_key(|profile| {
        let label = profile.label.to_ascii_uppercase();
        if label == "GENERAL" {
            return (0, String::new(), 0, profile.filename.clone());
        }
        let stem = label.trim_end_matches(|c: char| c.is_ascii_digit());
        let number = label[stem.len()..].parse::<u32>().unwrap_or(0);
        (1, stem.to_owned(), number, profile.filename.clone())
    });
    if profiles.is_empty() {
        return Err("В папке нет стратегий general*.bat.".into());
    }
    Ok(profiles)
}

fn settings_file() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(|root| PathBuf::from(root).join("ZapretUI").join("settings.txt"))
}

pub fn load() -> (PathBuf, String) {
    if let Some(saved) = settings_file().and_then(|file| fs::read_to_string(file).ok()) {
        if let Some((folder, selection)) = saved.split_once('\n') {
            let folder = PathBuf::from(folder);
            if folder.is_absolute() {
                return (folder, selection.to_owned());
            }
        }
    }

    let default = PathBuf::from(DEFAULT_FOLDER);
    if !default.is_dir() {
        if let Ok(exe) = env::current_exe() {
            if let Some(folder) = exe.parent() {
                if folder.join("service.bat").is_file() {
                    return (folder.to_owned(), String::new());
                }
            }
        }
    }
    (default, String::new())
}

pub fn save(folder: &Path, selection: &str) -> io::Result<()> {
    let file = settings_file().ok_or_else(|| io::Error::other("LOCALAPPDATA недоступен"))?;
    fs::create_dir_all(file.parent().unwrap())?;
    fs::write(file, format!("{}\n{}", folder.display(), selection))
}

pub fn launch_args(folder: &Path, filename: &str) -> Result<String, String> {
    let input = fs::read_to_string(folder.join(filename))
        .map_err(|err| format!("Не удалось прочитать стратегию. {err}"))?;
    let command = r#""%BIN%winws.exe""#;
    let unsupported = || "В стратегии не найдена поддерживаемая команда winws.exe.".to_owned();
    let mut lines = input.lines();
    let mut first = None;
    for line in lines.by_ref() {
        if line.trim_start().to_ascii_lowercase().starts_with("start ") {
            first = line.split_once(command).map(|(_, args)| args);
            break;
        }
    }
    let mut line = first.ok_or_else(unsupported)?;
    let mut source = String::new();
    loop {
        let trimmed = line.trim();
        if let Some(continuation) = trimmed.strip_suffix('^') {
            source.push_str(continuation.trim_end());
            source.push(' ');
            line = lines.next().ok_or_else(unsupported)?;
        } else {
            source.push_str(trimmed);
            break;
        }
    }
    let source = source.replace("^!", "!");
    if !source.starts_with("--") || source.contains(['^', '\0']) {
        return Err(unsupported());
    }

    let source = with_soundcloud_lists(&folder.join("lists"), source)?;

    let flag = folder.join("utils").join("game_filter.enabled");
    let (tcp, udp) = match fs::read_to_string(flag) {
        Ok(mode) => match mode
            .lines()
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "all" => ("1024-65535", "1024-65535"),
            "tcp" => ("1024-65535", "12"),
            _ => ("12", "1024-65535"),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => ("12", "12"),
        Err(err) => return Err(format!("Не удалось прочитать настройки Game Filter. {err}")),
    };

    let bin = format!("{}\\", folder.join("bin").display());
    let lists = format!("{}\\", folder.join("lists").display());
    let mut pieces = source.split('%');
    let mut args = pieces.next().unwrap_or_default().to_owned();
    while let Some(variable) = pieces.next() {
        args.push_str(match variable {
            "BIN" => &bin,
            "LISTS" => &lists,
            "GameFilterTCP" => tcp,
            "GameFilterUDP" => udp,
            _ => {
                return Err(format!(
                    "Неподдерживаемая переменная в стратегии: %{variable}%."
                ))
            }
        });
        args.push_str(pieces.next().ok_or_else(unsupported)?);
    }
    for (name, default) in [
        ("ipset-exclude-user.txt", "203.0.113.113/32\r\n"),
        (
            "list-general-user.txt",
            "# Never leave this file empty\r\ndomain.example.abc\r\n",
        ),
        ("list-exclude-user.txt", "domain.example.abc\r\n"),
    ] {
        use std::io::Write;
        let path = folder.join("lists").join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => file
                .write_all(default.as_bytes())
                .map_err(|err| format!("Не удалось записать {name}. {err}"))?,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(format!("Не удалось создать {name}. {err}")),
        }
    }
    Ok(args)
}

fn with_soundcloud_lists(lists: &Path, mut command: String) -> Result<String, String> {
    // Keep Flowseal and user lists intact. Attach the supplement to the same
    // strategy blocks as the corresponding upstream include list.
    for (upstream, supplement, filename, content) in [
        (
            r#"--hostlist="%LISTS%list-general.txt""#,
            r#"--hostlist="%LISTS%soundcloud-hosts.txt""#,
            "soundcloud-hosts.txt",
            include_bytes!("../assets/soundcloud/hosts.txt").as_slice(),
        ),
        (
            r#"--ipset="%LISTS%ipset-all.txt""#,
            r#"--ipset="%LISTS%soundcloud-ipset.txt""#,
            "soundcloud-ipset.txt",
            include_bytes!("../assets/soundcloud/ipset.txt").as_slice(),
        ),
    ] {
        if command.contains(upstream) {
            let path = lists.join(filename);
            if fs::read(&path).as_deref().ok() != Some(content) {
                fs::write(path, content).map_err(|err| {
                    format!("Не удалось записать список SoundCloud {filename}. {err}")
                })?;
            }
            command = command.replace(upstream, &format!("{upstream} {supplement}"));
        }
    }
    Ok(command)
}
