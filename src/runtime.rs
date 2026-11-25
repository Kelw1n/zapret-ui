use std::{
    env, fmt, fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Networking::WinHttp::*,
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
};
use zip::ZipArchive;

use crate::{profiles, wide};

const BUNDLED: Version = Version(1, 10, 4);
const CORE: &[u8] = include_bytes!("../assets/runtime/zapret-1.10.4.zip");
const DIVERT_LICENSE: &[u8] = include_bytes!("../assets/runtime/WinDivert-LICENSE.txt");
const CORE_LICENSE: &[u8] = include_bytes!("../assets/runtime/Zapret-LICENSE.txt");
const PROXY: &[u8] = include_bytes!("../assets/runtime/telegram-1.11.2.zip");
const ORIGIN: &str = "/Flowseal/zapret-discord-youtube/releases";
const BUNDLED_SHA256: &str = "291af673bd0238506f9548e0e94ca0abf244000f4c9a06960544fb01c5f97b34";
const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Version(u32, u32, u32);

impl Version {
    fn parse(tag: &str) -> Option<Self> {
        let mut segments = tag.split('.');
        let version = Self(
            segments.next()?.parse().ok()?,
            segments.next()?.parse().ok()?,
            segments.next()?.parse().ok()?,
        );
        (segments.next().is_none() && version.to_string() == tag).then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone)]
pub struct Installed {
    pub version: Version,
    pub folder: PathBuf,
}

#[derive(Clone)]
pub struct Release {
    pub version: Version,
    digest: String,
    size: usize,
}

pub struct ReleaseCheck {
    pub release: Release,
    pub offline: bool,
}

#[derive(Deserialize)]
struct Catalog {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: usize,
    digest: Option<String>,
    browser_download_url: String,
}

pub struct CoreState {
    pub active: Installed,
    pub previous: Option<Installed>,
}

pub fn data_root() -> Result<PathBuf, String> {
    env::var_os("LOCALAPPDATA")
        .map(|folder| PathBuf::from(folder).join("ZapretUI"))
        .filter(|folder| folder.is_absolute())
        .ok_or_else(|| "LOCALAPPDATA недоступен.".into())
}

fn core_root() -> Result<PathBuf, String> {
    Ok(data_root()?.join("runtime").join("zapret"))
}

fn read_installed(root: &Path, version: Version) -> Option<Installed> {
    let folder = root.join(version.to_string());
    (folder.join(".ready").is_file() && profiles::scan(&folder).is_ok())
        .then_some(Installed { version, folder })
}

pub fn prepare_core(previous: &Path) -> Result<CoreState, String> {
    let root = core_root()?;
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    match fs::read_to_string(root.join("active.txt")) {
        Ok(record) => {
            let mut lines = record.lines();
            let active = lines
                .next()
                .and_then(Version::parse)
                .and_then(|v| read_installed(&root, v));
            let prior = lines
                .next()
                .and_then(Version::parse)
                .and_then(|v| read_installed(&root, v));
            if let Some(active) = active {
                return Ok(CoreState {
                    active,
                    previous: prior,
                });
            }
            if let Some(active) = prior {
                save_state(&root, &active, None)?;
                return Ok(CoreState {
                    active,
                    previous: None,
                });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Version 0.3 used the newest complete directory as an implicit pointer.
            let mut legacy: Vec<_> = fs::read_dir(&root)
                .map_err(|err| err.to_string())?
                .flatten()
                .filter_map(|entry| Version::parse(entry.file_name().to_str()?))
                .filter_map(|version| read_installed(&root, version))
                .collect();
            legacy.sort_by_key(|item| item.version);
            if let Some(active) = legacy.pop() {
                let previous = legacy.pop();
                save_state(&root, &active, previous.as_ref())?;
                return Ok(CoreState { active, previous });
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    verify_archive(&bundled_release(), CORE)?;
    let active = install_core(&root, BUNDLED, CORE)?;
    carry_settings(previous, &active.folder)?;
    save_state(&root, &active, None)?;
    Ok(CoreState {
        active,
        previous: None,
    })
}

pub fn activate(next: &Installed, current: Option<&Installed>) -> Result<(), String> {
    profiles::scan(&next.folder)?;
    if let Some(current) = current {
        carry_settings(&current.folder, &next.folder)?;
    }
    save_state(&core_root()?, next, current)
}

fn carry_settings(previous: &Path, next: &Path) -> Result<(), String> {
    if previous == next {
        return Ok(());
    }
    for path in [
        "lists/ipset-exclude-user.txt",
        "lists/list-general-user.txt",
        "lists/list-exclude-user.txt",
        "utils/game_filter.enabled",
    ] {
        let old = previous.join(path);
        let destination = next.join(path);
        if old.is_file() {
            fs::copy(old, destination).map_err(|error| error.to_string())?;
        } else if path == "utils/game_filter.enabled" {
            match fs::remove_file(destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(())
}

fn save_state(root: &Path, active: &Installed, previous: Option<&Installed>) -> Result<(), String> {
    let record = format!(
        "{}\n{}\n",
        active.version,
        previous.map_or(String::new(), |item| item.version.to_string())
    );
    let path = root.join("active.txt");
    let staging = root.join("active.tmp");
    let file = fs::File::create(&staging).map_err(|err| err.to_string())?;
    use std::io::Write;
    (&file)
        .write_all(record.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| err.to_string())?;
    drop(file);
    check(unsafe {
        MoveFileExW(
            wide(staging.as_os_str()).as_ptr(),
            wide(path.as_os_str()).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    })
}

pub fn prepare_proxy() -> Result<PathBuf, String> {
    let runtime = data_root()?.join("runtime");
    fs::create_dir_all(&runtime).map_err(|err| err.to_string())?;
    let destination = runtime.join("telegram-1.11.2");
    install_folder(&destination, PROXY, None, |stage| {
        if !stage.join("python.exe").is_file() || !stage.join("proxy/tg_ws_proxy.py").is_file() {
            return Err("В архиве отсутствуют файлы TG WS Proxy.".into());
        }
        Ok(())
    })?;
    Ok(destination)
}

fn bundled_release() -> Release {
    Release {
        version: BUNDLED,
        digest: BUNDLED_SHA256.into(),
        size: CORE.len(),
    }
}

pub fn latest() -> Result<ReleaseCheck, String> {
    let response = download(
        "api.github.com",
        &format!("/repos{ORIGIN}/latest"),
        512 * 1024,
    );
    match response {
        Ok(bytes) => Ok(ReleaseCheck {
            release: parse_release(&bytes)?,
            offline: false,
        }),
        Err(_) => Ok(ReleaseCheck {
            release: bundled_release(),
            offline: true,
        }),
    }
}

fn artifact_name(version: Version) -> String {
    format!("zapret-discord-youtube-{version}.zip")
}

fn artifact_path(version: Version) -> String {
    format!("{ORIGIN}/download/{version}/{}", artifact_name(version))
}

fn parse_release(bytes: &[u8]) -> Result<Release, String> {
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
    let version = Version::parse(&catalog.tag_name)
        .filter(|_| !catalog.draft && !catalog.prerelease)
        .ok_or("Flowseal не вернул стабильную версию Zapret.")?;
    let name = artifact_name(version);
    let url = format!("https://github.com{}", artifact_path(version));
    let asset = catalog
        .assets
        .into_iter()
        .find(|asset| {
            asset.name == name
                && asset.browser_download_url == url
                && (1..=LIMIT).contains(&asset.size)
        })
        .ok_or("В релизе Flowseal нет ожидаемого ZIP-архива.")?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("GitHub не опубликовал SHA-256 для архива Flowseal.")?;
    Ok(Release {
        version,
        digest: digest.to_owned(),
        size: asset.size,
    })
}

fn verify_archive(release: &Release, archive: &[u8]) -> Result<(), String> {
    if archive.len() != release.size
        || !format!("{:x}", Sha256::digest(archive)).eq_ignore_ascii_case(&release.digest)
    {
        return Err("Размер или SHA-256 архива не совпал с данными релиза Flowseal.".into());
    }
    Ok(())
}

pub fn update(release: &Release) -> Result<Installed, String> {
    let archive = if release.version == BUNDLED && verify_archive(release, CORE).is_ok() {
        std::borrow::Cow::Borrowed(CORE)
    } else {
        std::borrow::Cow::Owned(download(
            "github.com",
            &artifact_path(release.version),
            release.size,
        )?)
    };
    verify_archive(release, &archive)?;
    install_core(&core_root()?, release.version, &archive)
}

fn install_core(root: &Path, version: Version, archive: &[u8]) -> Result<Installed, String> {
    let folder = root.join(version.to_string());
    if folder.join(".ready").is_file() && profiles::scan(&folder).is_err() {
        // An incomplete local installation can be rebuilt from the bundled archive.
        fs::remove_file(folder.join(".ready")).map_err(|err| err.to_string())?;
    }
    install_folder(
        &folder,
        archive,
        Some(&format!("zapret-discord-youtube-{version}")),
        |stage| {
            for item in ["bin/WinDivert.dll", "bin/WinDivert64.sys", "general.bat"] {
                if !stage.join(item).is_file() {
                    return Err(format!("В архиве отсутствует {item}."));
                }
            }
            fs::write(stage.join("LICENSE.txt"), CORE_LICENSE).map_err(|err| err.to_string())?;
            fs::write(stage.join("WinDivert-LICENSE.txt"), DIVERT_LICENSE)
                .map_err(|err| err.to_string())?;
            for profile in profiles::scan(stage)? {
                profiles::launch_args(stage, &profile.filename).map_err(|err| {
                    format!("Стратегия {} не поддерживается. {err}", profile.label)
                })?;
            }
            Ok(())
        },
    )?;
    Ok(Installed { version, folder })
}

fn install_folder(
    destination: &Path,
    bytes: &[u8],
    prefix: Option<&str>,
    validate: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if destination.join(".ready").is_file() {
        return Ok(());
    }
    let mut staging_name = destination
        .file_name()
        .ok_or("Некорректное имя каталога.")?
        .to_os_string();
    staging_name.push(".staging");
    let stage = destination.with_file_name(staging_name);
    clean(&stage).map_err(|err| err.to_string())?;
    fs::create_dir(&stage).map_err(|err| err.to_string())?;
    let result = (|| -> Result<(), String> {
        extract(&stage, bytes, prefix).map_err(|err| err.to_string())?;
        validate(&stage)?;
        fs::write(stage.join(".ready"), b"").map_err(|err| err.to_string())?;
        clean(destination).map_err(|err| err.to_string())?;
        fs::rename(&stage, destination).map_err(|err| err.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = clean(&stage);
    }
    result
}

fn clean(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn extract(target: &Path, bytes: &[u8], prefix: Option<&str>) -> io::Result<()> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;
    let mut remaining = 100 * 1024 * 1024u64;
    if zip.len() > 3000 {
        return Err(io::Error::other("Слишком много файлов в архиве."));
    }
    for index in 0..zip.len() {
        let file = zip.by_index(index)?;
        let unsafe_path = || io::Error::other("Недопустимый путь в архиве.");
        let path = file.enclosed_name().ok_or_else(unsafe_path)?;
        let relative = match prefix {
            Some(prefix) => path.strip_prefix(prefix).map_err(|_| unsafe_path())?,
            None => &path,
        };
        if relative.as_os_str().is_empty() && file.is_dir() {
            continue;
        }
        if !relative.components().all(|component| {
            matches!(component, Component::Normal(name) if safe_component(&name.to_string_lossy()))
        }) || file.is_symlink() {
            return Err(unsafe_path());
        }
        let path = target.join(relative);
        if file.is_dir() {
            fs::create_dir_all(path)?;
        } else {
            if file.size() > remaining {
                return Err(io::Error::other("Архив превышает лимит распаковки."));
            }
            fs::create_dir_all(path.parent().ok_or_else(unsafe_path)?)?;
            let mut output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)?;
            let size = io::copy(&mut file.take(remaining + 1), &mut output)?;
            remaining = remaining
                .checked_sub(size)
                .ok_or_else(|| io::Error::other("Архив превышает лимит распаковки."))?;
        }
    }
    Ok(())
}

fn safe_component(name: &str) -> bool {
    if name.ends_with([' ', '.']) || name.contains([':', '\\']) {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    !base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

struct Http(*mut std::ffi::c_void);

impl Http {
    fn open(raw: *mut std::ffi::c_void) -> Result<Self, String> {
        if raw.is_null() {
            Err(io::Error::last_os_error().to_string())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn check(result: i32) -> Result<(), String> {
    if result == 0 {
        Err(io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

fn download(host: &str, path: &str, limit: usize) -> Result<Vec<u8>, String> {
    unsafe {
        let agent = wide(concat!("ZapretUI/", env!("CARGO_PKG_VERSION")));
        let proxy = env::var("HTTPS_PROXY")
            .or_else(|_| env::var("https_proxy"))
            .ok()
            .and_then(|url| {
                let authority = url.strip_prefix("http://")?.trim_end_matches('/');
                (!authority.is_empty() && !authority.contains(['@', '/', '?', '#', ' ', ';', '=']))
                    .then(|| wide(authority))
            });
        let (access, proxy) = match &proxy {
            Some(proxy) => (WINHTTP_ACCESS_TYPE_NAMED_PROXY, proxy.as_ptr()),
            None => (WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null()),
        };
        let session = Http::open(WinHttpOpen(agent.as_ptr(), access, proxy, null(), 0))?;
        check(WinHttpSetTimeouts(session.0, 8_000, 8_000, 10_000, 10_000))?;
        let connection = Http::open(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0))?;
        let request = Http::open(WinHttpOpenRequest(
            connection.0,
            null(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP;
        check(WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&policy as *const u32).cast(),
            4,
        ))?;
        check(WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0))?;
        check(WinHttpReceiveResponse(request.0, null_mut()))?;
        let mut status = 0u32;
        let mut length = 4;
        check(WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut length,
            null_mut(),
        ))?;
        if status != 200 {
            return Err(format!("GitHub ответил HTTP {status}."));
        }
        let started = Instant::now();
        let mut data = Vec::new();
        let mut buffer = [0u8; 16384];
        loop {
            let mut received = 0;
            check(WinHttpReadData(
                request.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut received,
            ))?;
            if started.elapsed() > Duration::from_secs(60) {
                return Err("Истекло время загрузки с GitHub.".into());
            }
            if received == 0 {
                return Ok(data);
            }
            if data.len() + received as usize > limit {
                return Err("Ответ GitHub превышает допустимый размер.".into());
            }
            data.extend_from_slice(&buffer[..received as usize]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_archive_matches_digest_and_rejects_tampering() {
        let release = bundled_release();
        verify_archive(&release, CORE).unwrap();
        let mut changed = CORE.to_vec();
        changed[0] ^= 1;
        assert!(verify_archive(&release, &changed).is_err());
    }

    #[test]
    fn rejects_prereleases_unofficial_assets_and_missing_checksums() {
        let mut metadata = serde_json::json!({
            "tag_name": "1.10.4",
            "draft": false,
            "prerelease": false,
            "assets": [{
                "name": artifact_name(BUNDLED),
                "size": CORE.len(),
                "digest": format!("sha256:{BUNDLED_SHA256}"),
                "browser_download_url": format!("https://github.com{}", artifact_path(BUNDLED)),
            }]
        });
        let parse = |value: &serde_json::Value| parse_release(&serde_json::to_vec(value).unwrap());
        verify_archive(&parse(&metadata).unwrap(), CORE).unwrap();
        metadata["prerelease"] = true.into();
        assert!(parse(&metadata).is_err());
        metadata["prerelease"] = false.into();
        metadata["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(parse(&metadata).is_err());
        metadata["assets"][0]["digest"] = format!("sha256:{BUNDLED_SHA256}").into();
        metadata["assets"][0]["browser_download_url"] = "https://example.com/asset.zip".into();
        assert!(parse(&metadata).is_err());
    }
}
