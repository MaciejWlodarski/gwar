//! Login registration format and effective state (no shell interpolation).
use serde::Serialize;
use tauri::AppHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginState {
    Disabled,
    Enabled,
    #[cfg(target_os = "macos")]
    ApprovalRequired,
    #[cfg(target_os = "macos")]
    Pending,
}

#[cfg(any(windows, test))]
fn windows_command(executable: &str) -> Result<String, String> {
    // Windows filenames cannot contain quotes or NUL. There are no startup arguments.
    if executable.is_empty() || executable.contains(['"', '\0', '\r', '\n']) || executable.ends_with('\\') {
        return Err("Invalid startup executable path".into());
    }
    Ok(format!("\"{executable}\""))
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent(executable: &str) -> Result<Vec<u8>, String> {
    use plist::Value;
    let value = Value::Dictionary(plist::Dictionary::from_iter([
        ("Label", Value::String("Gwar".into())),
        ("AssociatedBundleIdentifiers", Value::Array(vec![Value::String("app.gwar.desktop".into())])),
        ("ProgramArguments", Value::Array(vec![Value::String(executable.into())])),
        ("RunAtLoad", Value::Boolean(true)),
    ]));
    let mut bytes = Vec::new();
    value.to_writer_xml(&mut bytes).map_err(|e| e.to_string())?;
    // Refuse any filename the XML representation cannot preserve exactly.
    if plist::Value::from_reader_xml(bytes.as_slice()).map_err(|e| e.to_string())? != value {
        return Err("Startup executable path cannot be represented in a plist".into());
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2_foundation::{NSString, NSURL};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tauri::Manager;

    fn path(app: &AppHandle) -> Result<PathBuf, String> {
        // Keep the former plugin's filename and Label, avoiding duplicate login entries.
        Ok(app.path().home_dir().map_err(|e| e.to_string())?.join("Library/LaunchAgents/Gwar.plist"))
    }

    pub(super) fn status(app: &AppHandle) -> Result<LoginState, String> {
        effective_state(&path(app)?, |path| {
            let path_string = path.to_str().ok_or("Startup plist path is not valid Unicode")?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(path_string));
            // macOS 13+ is checked by supported() before this module is called.
            // statusForLegacyURL: is Apple's Objective-C spelling of statusForLegacyPlist(at:).
            Ok(unsafe { SMAppService::statusForLegacyURL(&url) })
        })
    }

    fn effective_state(
        path: &Path,
        query: impl FnOnce(&Path) -> Result<SMAppServiceStatus, String>,
    ) -> Result<LoginState, String> {
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(LoginState::Disabled);
        }
        let authorization = query(path)?;
        let state = match authorization {
            SMAppServiceStatus::Enabled => LoginState::Enabled,
            SMAppServiceStatus::RequiresApproval => LoginState::ApprovalRequired,
            // A file awaiting OS discovery is not proof that it is allowed to run.
            SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => LoginState::Pending,
            _ => return Err("Unrecognized macOS startup authorization status".into()),
        };
        Ok(state)
    }

    pub(super) fn set(app: &AppHandle, enabled: bool) -> Result<(), String> {
        let path = path(app)?;
        if enabled {
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            let bytes = launch_agent(executable.to_str().ok_or("Startup executable path is not valid Unicode")?)?;
            fs::create_dir_all(path.parent().ok_or("Invalid startup directory")?).map_err(|e| e.to_string())?;
            let temporary = path.with_extension("plist.tmp");
            fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
            fs::rename(temporary, path).map_err(|e| e.to_string())?;
        } else {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        // Never launch a second copy, override macOS consent, or open Settings here.
        Ok(())
    }

    #[cfg(test)]
    #[test]
    fn s2_existing_launch_agent_requires_effective_macos_authorization() {
        let dir = std::env::temp_dir().join(format!("gwar-startup-status-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Gwar.plist");
        fs::write(&path, launch_agent("/Applications/Gwar.app/Contents/MacOS/gwar").unwrap()).unwrap();
        let queried = std::cell::Cell::new(false);
        let blocked = effective_state(&path, |queried_path| {
            assert_eq!(queried_path, path);
            queried.set(true);
            Ok(SMAppServiceStatus::RequiresApproval)
        })
        .unwrap();
        assert_eq!(blocked, LoginState::ApprovalRequired);
        assert!(queried.get(), "an existing plist must still query OS authorization");
        assert_eq!(effective_state(&path, |_| Ok(SMAppServiceStatus::Enabled)).unwrap(), LoginState::Enabled);
        for status in [SMAppServiceStatus::NotFound, SMAppServiceStatus::NotRegistered] {
            assert_eq!(effective_state(&path, |_| Ok(status)).unwrap(), LoginState::Pending);
        }
        assert!(effective_state(&path, |_| Err("query failed".into())).is_err());
        fs::remove_file(&path).unwrap();
        assert_eq!(effective_state(&path, |_| panic!("no registration to query")).unwrap(), LoginState::Disabled);
        fs::remove_dir_all(dir).unwrap();
    }

    pub(super) fn open_settings() {
        // The caller checks OS availability; only an explicit settings button calls this.
        unsafe { SMAppService::openSystemSettingsLoginItems() };
    }
}

#[cfg(any(windows, test))]
fn windows_approved(bytes: Option<&[u8]>) -> Result<bool, String> {
    match bytes {
        None => Ok(true),
        Some(bytes) if bytes.len() == 12 => match u32::from_le_bytes(bytes[..4].try_into().unwrap()) {
            2 | 6 => Ok(true),
            3 | 7 => Ok(false),
            _ => Err("Unrecognized Windows startup approval state".into()),
        },
        Some(_) => Err("Invalid Windows startup approval state".into()),
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE};
    const RUN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    fn optional<T>(result: windows_registry::Result<T>) -> Result<Option<T>, String> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.code().0 as u32 == 0x80070002 => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn entry(root: &Key) -> Result<Option<String>, String> {
        optional(root.open(RUN).and_then(|key| key.get_string("Gwar")))
    }

    pub(super) fn status(_: &AppHandle) -> Result<LoginState, String> {
        let mut enabled = false;
        for root in [CURRENT_USER, LOCAL_MACHINE] {
            if let Some(command) = entry(root)? {
                if command.trim().is_empty() {
                    continue;
                }
                let approval = optional(root.open(APPROVED).and_then(|key| key.get_value("Gwar")))?;
                enabled |= windows_approved(approval.as_ref().map(|value| value.as_ref()))?;
            }
        }
        Ok(if enabled { LoginState::Enabled } else { LoginState::Disabled })
    }

    fn write_command(key: &Key, command: &str) -> Result<(), String> {
        key.set_string("Gwar", command).map_err(|e| e.to_string())
    }

    pub(super) fn set(_: &AppHandle, enabled: bool) -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = windows_command(executable.to_str().ok_or("Startup executable path is not valid Unicode")?)?;
        // The former plugin preferred HKLM when elevated. Update an existing entry in
        // that hive too; report access failures rather than leave an unsafe legacy value.
        let machine = entry(LOCAL_MACHINE)?.is_some();
        for (root, is_machine) in [(LOCAL_MACHINE, true), (CURRENT_USER, false)] {
            if is_machine && !machine {
                continue;
            }
            if enabled {
                if machine && !is_machine && entry(root)?.is_none() {
                    continue;
                }
                write_command(&root.create(RUN).map_err(|e| e.to_string())?, &command)?;
                if let Some(key) = optional(root.options().write().open(APPROVED))? {
                    let mut value = [0u8; 12];
                    value[0] = 2;
                    key.set_bytes("Gwar", windows_registry::Type::Bytes, &value).map_err(|e| e.to_string())?;
                }
            } else {
                optional(root.options().write().open(RUN).and_then(|key| key.remove_value("Gwar")))?;
            }
        }
        Ok(())
    }
    #[cfg(test)]
    #[test]
    fn s1_registry_round_trip_preserves_quoted_spaced_executable() {
        // An isolated test key: never the user's actual login registration.
        let path = format!(r"Software\Gwar\StartupTests\{}", std::process::id());
        let key = CURRENT_USER.create(&path).unwrap();
        let command = windows_command(r"C:\Program Files\Games & Friends\Gwar\gwar.exe").unwrap();
        write_command(&key, &command).unwrap();
        let actual = key.get_string("Gwar").unwrap();
        drop(key);
        CURRENT_USER.remove_tree(path).unwrap();
        assert_eq!(actual, r#""C:\Program Files\Games & Friends\Gwar\gwar.exe""#);
    }
}

pub fn status(app: &AppHandle) -> Result<LoginState, String> {
    #[cfg(target_os = "macos")]
    {
        macos::status(app)
    }
    #[cfg(windows)]
    {
        windows::status(app)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = app;
        Err("Login startup is unavailable on this platform".into())
    }
}

pub fn set(app: &AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::set(app, enabled)
    }
    #[cfg(windows)]
    {
        windows::set(app, enabled)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (app, enabled);
        Err("Login startup is unavailable on this platform".into())
    }
}

pub fn open_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::open_settings();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    Err("Login settings are unavailable on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s1_windows_spaced_executable_is_one_quoted_program() {
        assert_eq!(windows_command(r"C:\Program Files\Gwar\gwar.exe").unwrap(), r#""C:\Program Files\Gwar\gwar.exe""#);
        assert_eq!(windows_command(r"C:\Apps & Games\Gwar\gwar.exe").unwrap(), r#""C:\Apps & Games\Gwar\gwar.exe""#);
    }
    #[test]
    fn windows_rejects_unrepresentable_program_names_and_honors_task_manager() {
        for path in ["", "bad\"path.exe", "bad\0path.exe", "bad\npath.exe"] {
            assert!(windows_command(path).is_err());
        }
        assert!(windows_approved(None).unwrap());
        assert!(windows_approved(Some(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap());
        assert!(!windows_approved(Some(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap());
        assert!(windows_approved(Some(&[3])).is_err());
    }
    #[test]
    fn s3_launch_agent_round_trips_special_install_path() {
        let executable = "/Applications/Games & Friends <2026> \"Gwar\" 'Łódź'.app/Contents/MacOS/gwar";
        let bytes = launch_agent(executable).unwrap();
        let value = plist::Value::from_reader_xml(bytes.as_slice()).expect("well-formed escaped XML");
        let dict = value.as_dictionary().unwrap();
        assert_eq!(dict["ProgramArguments"].as_array().unwrap(), &[plist::Value::String(executable.into())]);
        assert_eq!(dict["Label"].as_string(), Some("Gwar"));
        assert_eq!(dict["RunAtLoad"].as_boolean(), Some(true));
    }
}
