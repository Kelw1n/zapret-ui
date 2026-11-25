use std::{
    fs, io,
    net::{Ipv4Addr, TcpListener, TcpStream},
    ptr::null_mut,
    time::Duration,
};

use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

use crate::process::Session;

pub const PORT: u16 = 1443;
const HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

pub struct Proxy {
    pub session: Session,
    pub port: u16,
    secret: String,
}

impl Proxy {
    pub fn start(port: u16) -> Result<Self, String> {
        let module = crate::runtime::prepare_proxy()?;
        TcpListener::bind((HOST, port)).map_err(|err| format!("Порт {port} недоступен. {err}"))?;
        let secret = secret().map_err(|err| format!("Ошибка настройки прокси. {err}"))?;
        let args = format!(
            r#"-I -B -c "import os,sys; sys.stderr=sys.stdout=open(os.devnull,'w'); from proxy.tg_ws_proxy import main; main()" --host {HOST} --port {port} --secret {secret}"#
        );
        let session = Session::start(&module.join("python.exe"), &module, args.as_ref())
            .map_err(|err| format!("Ошибка запуска TG WS Proxy. {err}"))?;
        Ok(Self {
            session,
            port,
            secret,
        })
    }

    pub fn listening(&self) -> bool {
        TcpStream::connect_timeout(&(HOST, self.port).into(), Duration::from_millis(60)).is_ok()
    }

    pub fn link(&self) -> String {
        format!(
            "tg://proxy?server={HOST}&port={}&secret=dd{}",
            self.port, self.secret
        )
    }
}

fn secret() -> io::Result<String> {
    let folder = crate::runtime::data_root().map_err(io::Error::other)?;
    let path = folder.join("telegram-secret.txt");
    match fs::read_to_string(&path) {
        Ok(stored) if stored.len() == 32 && stored.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            return Ok(stored)
        }
        Ok(_) => return Err(io::Error::other("Некорректный ключ в telegram-secret.txt")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom: 0x{:08X}",
            status as u32
        )));
    }
    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    fs::create_dir_all(folder)?;
    fs::write(path, &key)?;
    Ok(key)
}
