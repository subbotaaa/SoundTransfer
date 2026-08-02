//! Автообновление через GitHub Releases (репозиторий публичный —
//! проверка и скачивание не требуют токена).

use anyhow::{Context, Result, bail};

const RELEASES_API: &str =
    "https://api.github.com/repos/subbotaaa/SoundTransfer/releases/latest";

#[cfg(windows)]
const ASSET_NAME: &str = "SoundTransfer-windows.zip";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "SoundTransfer-macos.zip";
#[cfg(not(any(windows, target_os = "macos")))]
const ASSET_NAME: &str = "";

pub fn current_version() -> String {
    // ST_FAKE_VERSION — для теста самообновления без даунгрейда сборки.
    std::env::var("ST_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

#[derive(Debug, Clone)]
pub struct Latest {
    pub version: String,
    pub asset_url: String,
}

fn parse_ver(v: &str) -> (u64, u64, u64) {
    let mut it = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Возвращает Some(..), если на GitHub есть версия новее текущей.
pub fn check() -> Result<Option<Latest>> {
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        assets: Vec<Asset>,
    }

    let body = ureq::get(RELEASES_API)
        .header("User-Agent", "SoundTransfer-updater")
        .call()
        .context("запрос GitHub Releases")?
        .body_mut()
        .read_to_string()
        .context("чтение ответа")?;
    let release: Release = serde_json::from_str(&body).context("разбор ответа GitHub")?;

    if parse_ver(&release.tag_name) <= parse_ver(&current_version()) {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == ASSET_NAME)
        .with_context(|| format!("в релизе {} нет {ASSET_NAME}", release.tag_name))?;
    Ok(Some(Latest {
        version: release.tag_name,
        asset_url: asset.browser_download_url,
    }))
}

fn download(url: &str) -> Result<Vec<u8>> {
    Ok(ureq::get(url)
        .header("User-Agent", "SoundTransfer-updater")
        .call()
        .context("скачивание обновления")?
        .body_mut()
        .with_config()
        .limit(200 * 1024 * 1024)
        .read_to_vec()?)
}

/// Удаляет остатки предыдущего обновления (вызывать при старте).
pub fn cleanup_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_file(dir.join("st_old.exe"));
        }
        #[cfg(target_os = "macos")]
        if let Some(bundle) = bundle_root(&exe) {
            if let Some(parent) = bundle.parent() {
                let _ = std::fs::remove_dir_all(parent.join("SoundTransfer.app.old"));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn bundle_root(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    // .../SoundTransfer.app/Contents/MacOS/st → корень бандла
    let bundle = exe.parent()?.parent()?.parent()?;
    if bundle.extension()? == "app" {
        Some(bundle.to_path_buf())
    } else {
        None
    }
}

/// Скачивает и устанавливает обновление, затем перезапускает приложение.
/// При успехе НЕ возвращается (process::exit).
pub fn install(latest: &Latest) -> Result<()> {
    let data = download(&latest.asset_url)?;
    log::info!(
        "обновление {}: скачано {} КБ, устанавливаю…",
        latest.version,
        data.len() / 1024
    );
    #[cfg(windows)]
    return install_windows(data);
    #[cfg(target_os = "macos")]
    return install_macos(data);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = data;
        bail!("автообновление не поддерживается на этой ОС")
    }
}

#[cfg(windows)]
fn install_windows(data: Vec<u8>) -> Result<()> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).context("чтение архива")?;
    let mut exe_bytes = Vec::new();
    let mut found = false;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.name().ends_with("st.exe") {
            f.read_to_end(&mut exe_bytes)?;
            found = true;
            break;
        }
    }
    if !found {
        bail!("st.exe не найден в архиве");
    }

    let cur = std::env::current_exe().context("путь текущего exe")?;
    let dir = cur.parent().context("каталог exe")?;
    let new_path = dir.join("st_new.exe");
    std::fs::write(&new_path, &exe_bytes).context("запись нового exe")?;

    // Работающий exe нельзя перезаписать, но можно переименовать.
    let old_path = dir.join("st_old.exe");
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(&cur, &old_path).context("переименование текущего exe")?;
    if let Err(e) = std::fs::rename(&new_path, &cur) {
        let _ = std::fs::rename(&old_path, &cur); // откат
        return Err(e).context("установка нового exe");
    }
    std::process::Command::new(&cur)
        .spawn()
        .context("перезапуск")?;
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn install_macos(data: Vec<u8>) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    let cur_exe = std::env::current_exe().context("путь текущего бинарника")?;
    let bundle =
        bundle_root(&cur_exe).context("приложение запущено не из .app — обновите вручную")?;
    let parent = bundle.parent().context("родительский каталог .app")?;

    // Распаковываем во временный каталог РЯДОМ с бандлом (тот же том — rename сработает).
    let new_bundle = parent.join("SoundTransfer.app.new");
    let _ = std::fs::remove_dir_all(&new_bundle);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).context("чтение архива")?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        let Some(rel) = name.strip_prefix("SoundTransfer.app/") else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let out = new_bundle.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(pp) = out.parent() {
            std::fs::create_dir_all(pp)?;
        }
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        std::fs::write(&out, &bytes)?;
        if let Some(mode) = f.unix_mode() {
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
        }
    }

    let new_exe = new_bundle.join("Contents/MacOS/st");
    if !new_exe.exists() {
        let _ = std::fs::remove_dir_all(&new_bundle);
        bail!("в архиве нет Contents/MacOS/st");
    }
    let _ = std::fs::set_permissions(&new_exe, std::fs::Permissions::from_mode(0o755));

    let old_bundle = parent.join("SoundTransfer.app.old");
    let _ = std::fs::remove_dir_all(&old_bundle);
    std::fs::rename(&bundle, &old_bundle).context("переименование текущего .app")?;
    if let Err(e) = std::fs::rename(&new_bundle, &bundle) {
        let _ = std::fs::rename(&old_bundle, &bundle); // откат
        return Err(e).context("установка нового .app");
    }
    std::process::Command::new("open")
        .arg("-n")
        .arg(&bundle)
        .spawn()
        .context("перезапуск")?;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(parse_ver("v1.2.3"), (1, 2, 3));
        assert_eq!(parse_ver("0.10.0"), (0, 10, 0));
        assert!(parse_ver("v0.2.0") > parse_ver("0.1.9"));
        assert!(parse_ver("v0.10.0") > parse_ver("v0.9.9"));
        assert!(parse_ver("1.0.0") > parse_ver("0.99.99"));
    }
}
