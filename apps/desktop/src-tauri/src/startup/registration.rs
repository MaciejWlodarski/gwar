//! Login registration format and effective state (no shell interpolation).
use serde::Serialize;
use tauri::AppHandle;

mod model;
use model::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginState {
    Disabled,
    Enabled,
    #[cfg(target_os = "macos")]
    ApprovalRequired,
    #[cfg(target_os = "macos")]
    Pending,
    #[cfg(target_os = "macos")]
    Legacy,
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
    use model::{Entry, Snapshot};
    use objc2_foundation::{NSString, NSURL};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tauri::Manager;

    struct PlistEntry<Q> {
        path: PathBuf,
        major: isize,
        authorization: Q,
    }
    impl<Q: FnMut(&Path) -> Result<LoginState, String>> Entry for PlistEntry<Q> {
        type Value = Vec<u8>;
        fn read(&mut self) -> Result<Option<Vec<u8>>, String> {
            match fs::read(&self.path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }
        fn retarget(&self, bytes: &Vec<u8>, executable: &str) -> Result<Vec<u8>, String> {
            model::macos_retarget(bytes, executable)
        }
        fn write(&mut self, bytes: &Vec<u8>) -> Result<(), String> {
            let temporary = self.path.with_extension("plist.tmp");
            fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
            fs::rename(temporary, &self.path).map_err(|e| e.to_string())
        }
        fn state(&mut self, bytes: &Vec<u8>) -> Result<LoginState, String> {
            if self.major < 13 {
                return Ok(LoginState::Legacy);
            }
            if model::macos_disabled(bytes)? {
                return Ok(LoginState::Disabled);
            }
            (self.authorization)(&self.path)
        }
    }

    fn path(app: &AppHandle) -> Result<PathBuf, String> {
        Ok(app.path().home_dir().map_err(|e| e.to_string())?.join("Library/LaunchAgents/Gwar.plist"))
    }
    fn query(path: &Path) -> Result<LoginState, String> {
        let path_string = path.to_str().ok_or("Startup plist path is not valid Unicode")?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_string));
        // Only PlistEntry::state on macOS 13+ calls this selector.
        // statusForLegacyURL: is Apple's Objective-C name for statusForLegacyPlist(at:).
        match unsafe { SMAppService::statusForLegacyURL(&url) } {
            SMAppServiceStatus::Enabled => Ok(LoginState::Enabled),
            SMAppServiceStatus::RequiresApproval => Ok(LoginState::ApprovalRequired),
            SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => Ok(LoginState::Pending),
            _ => Err("Unrecognized macOS startup authorization status".into()),
        }
    }
    type AuthorizationQuery = fn(&Path) -> Result<LoginState, String>;
    fn entry(app: &AppHandle) -> Result<PlistEntry<AuthorizationQuery>, String> {
        Ok(PlistEntry { path: path(app)?, major: super::super::macos_major(), authorization: query })
    }
    pub(super) fn status(app: &AppHandle, executable: &str) -> Result<Snapshot, String> {
        model::reconcile(&mut entry(app)?, executable)
    }
    pub(super) fn set(app: &AppHandle, executable: &str, enabled: bool) -> Result<(), String> {
        set_entry(&mut entry(app)?, executable, enabled)
    }
    fn set_entry<Q: FnMut(&Path) -> Result<LoginState, String>>(
        entry: &mut PlistEntry<Q>,
        executable: &str,
        enabled: bool,
    ) -> Result<(), String> {
        if enabled && entry.major < 13 {
            return Err("This macOS version can remove saved startup but cannot verify enabling it".into());
        }
        let prior = model::repair(entry, executable)?;
        if enabled {
            let bytes = if let Some(bytes) = prior {
                let mut value = model::macos_value(&bytes)?;
                let dict = value.as_dictionary_mut().ok_or("Invalid startup plist")?;
                dict.insert("RunAtLoad".into(), plist::Value::Boolean(true));
                if dict.contains_key("Disabled") {
                    dict.insert("Disabled".into(), plist::Value::Boolean(false));
                }
                let mut bytes = Vec::new();
                value.to_writer_xml(&mut bytes).map_err(|e| e.to_string())?;
                bytes
            } else {
                model::macos_retarget(&launch_agent(executable)?, executable)?
            };
            fs::create_dir_all(entry.path.parent().ok_or("Invalid startup directory")?).map_err(|e| e.to_string())?;
            entry.write(&bytes)?;
        } else if prior.is_some() {
            fs::remove_file(&entry.path).map_err(|e| e.to_string())?;
            if entry.read()?.is_some() {
                return Err("Startup removal failed read-back validation".into());
            }
        }
        Ok(())
    }
    pub(super) fn open_settings() {
        unsafe { SMAppService::openSystemSettingsLoginItems() };
    }

    #[cfg(test)]
    #[test]
    fn r3_s2_filesystem_repairs_before_authorization_and_preserves_disabled_flags() {
        let dir = std::env::temp_dir().join(format!("gwar-startup-migration-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Gwar.plist");
        let current = "/Applications/New & Friends/Gwar.app/Contents/MacOS/gwar";
        let old = launch_agent("/Applications/Old/Gwar.app/Contents/MacOS/gwar").unwrap();
        fs::write(&path, &old).unwrap();
        let mut entry = PlistEntry {
            path: path.clone(),
            major: 13,
            authorization: |queried: &Path| {
                let bytes = fs::read(queried).unwrap();
                assert_eq!(
                    model::macos_retarget(&bytes, current).unwrap(),
                    bytes,
                    "target must be repaired before asking authorization"
                );
                Ok(LoginState::ApprovalRequired)
            },
        };
        assert_eq!(model::reconcile(&mut entry, current).unwrap().state, LoginState::ApprovalRequired);
        let mut disabled = plist::Value::from_reader_xml(old.as_slice()).unwrap();
        disabled.as_dictionary_mut().unwrap().insert("Disabled".into(), plist::Value::Boolean(true));
        disabled.to_file_xml(&path).unwrap();
        assert_eq!(model::reconcile(&mut entry, current).unwrap().state, LoginState::Disabled);
        assert!(model::macos_disabled(&fs::read(&path).unwrap()).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(test)]
    #[test]
    fn r3_s3_old_macos_keeps_owned_entry_without_querying_unavailable_api() {
        let dir = std::env::temp_dir().join(format!("gwar-startup-old-macos-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Gwar.plist");
        let current = "/Applications/Gwar.app/Contents/MacOS/gwar";
        for major in [11, 12] {
            let before = launch_agent(current).unwrap();
            fs::write(&path, &before).unwrap();
            let mut entry = PlistEntry {
                path: path.clone(),
                major,
                authorization: |_: &Path| panic!("SMAppService is unavailable"),
            };
            assert_eq!(
                model::reconcile(&mut entry, current).unwrap(),
                Snapshot { present: true, state: LoginState::Legacy }
            );
            assert!(path.exists(), "reading never deletes an existing user choice");
            assert!(set_entry(&mut entry, current, true).is_err());
            assert_eq!(fs::read(&path).unwrap(), before, "unverifiable enable must leave the choice unchanged");
            set_entry(&mut entry, current, false).unwrap();
            assert!(!path.exists(), "the existing entry can still be removed");
            assert!(set_entry(&mut entry, current, true).is_err());
            assert!(!path.exists(), "removal-only mode never creates a registration");
        }
        let mut invalid =
            PlistEntry { path: path.clone(), major: 13, authorization: |_: &Path| panic!("not registered") };
        assert!(set_entry(&mut invalid, "/tmp/gwar-desktop", true).is_err());
        assert!(!path.exists(), "an unrecognized new target must not create an entry");
        fs::remove_dir_all(dir).unwrap();
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

    fn read_command(root: &Key, path: &str) -> Result<Option<String>, String> {
        let Some(value) = optional(root.open(path).and_then(|key| key.get_value("Gwar")))? else {
            return Ok(None);
        };
        if value.ty() != windows_registry::Type::String {
            return Err("Startup entry has an unrecognized registry type; left unchanged".into());
        }
        String::try_from(value).map(Some).map_err(|e| e.to_string())
    }
    fn entry(root: &Key) -> Result<Option<String>, String> {
        read_command(root, RUN)
    }

    struct RunEntry<'a> {
        root: &'a Key,
        run: &'a str,
        approved: &'a str,
    }
    impl<'a> RunEntry<'a> {
        fn new(root: &'a Key) -> Self {
            Self { root, run: RUN, approved: APPROVED }
        }
    }
    impl model::Entry for RunEntry<'_> {
        type Value = String;
        fn read(&mut self) -> Result<Option<String>, String> {
            read_command(self.root, self.run)
        }
        fn retarget(&self, value: &String, executable: &str) -> Result<String, String> {
            model::windows_retarget(value, executable)
        }
        fn write(&mut self, value: &String) -> Result<(), String> {
            let key = self.root.options().write().open(self.run).map_err(|e| e.to_string())?;
            write_command(&key, value)
        }
        fn state(&mut self, _: &String) -> Result<LoginState, String> {
            let approval = optional(self.root.open(self.approved).and_then(|key| key.get_value("Gwar")))?;
            Ok(if windows_approved(approval.as_ref().map(|value| value.as_ref()))? {
                LoginState::Enabled
            } else {
                LoginState::Disabled
            })
        }
    }
    pub(super) fn status(_: &AppHandle, executable: &str) -> Result<Snapshot, String> {
        let mut snapshot = Snapshot { present: false, state: LoginState::Disabled };
        for root in [CURRENT_USER, LOCAL_MACHINE] {
            let current = model::reconcile(&mut RunEntry::new(root), executable)?;
            snapshot.present |= current.present;
            if current.state == LoginState::Enabled {
                snapshot.state = LoginState::Enabled;
            }
        }
        Ok(snapshot)
    }

    fn write_command(key: &Key, command: &str) -> Result<(), String> {
        key.set_string("Gwar", command).map_err(|e| e.to_string())
    }

    pub(super) fn set(_: &AppHandle, executable: &str, enabled: bool) -> Result<(), String> {
        for root in [CURRENT_USER, LOCAL_MACHINE] {
            model::repair(&mut RunEntry::new(root), executable)?;
        }
        let command = model::windows_retarget(&windows_command(executable)?, executable)?;
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
    fn r3_s1_registry_migration_quotes_target_without_changing_approval() {
        let path = format!(r"Software\Gwar\StartupTests\{}", std::process::id());
        let run = format!(r"{path}\Run");
        let approved = format!(r"{path}\Approved");
        let run_key = CURRENT_USER.create(&run).unwrap();
        let current = r"C:\Program Files\Gwar\gwar.exe";
        run_key.set_string("Gwar", format!("{current} ")).unwrap();
        let approved_key = CURRENT_USER.create(&approved).unwrap();
        let approval = [3u8, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        approved_key.set_bytes("Gwar", windows_registry::Type::Bytes, &approval).unwrap();
        let mut entry = RunEntry { root: CURRENT_USER, run: &run, approved: &approved };
        assert_eq!(model::reconcile(&mut entry, current).unwrap().state, LoginState::Disabled);
        assert_eq!(run_key.get_string("Gwar").unwrap(), format!("\"{current}\""));
        assert_eq!(approved_key.get_value("Gwar").unwrap().as_ref(), &approval);
        run_key.set_expand_string("Gwar", windows_command(current).unwrap()).unwrap();
        assert!(model::reconcile(&mut entry, current).is_err(), "only literal REG_SZ targets are verified");
        drop(run_key);
        drop(approved_key);
        CURRENT_USER.remove_tree(path).unwrap();
    }
}

#[cfg(any(target_os = "macos", windows))]
fn executable() -> Result<String, String> {
    std::env::current_exe()
        .map_err(|e| e.to_string())?
        .into_os_string()
        .into_string()
        .map_err(|_| "Startup executable path is not valid Unicode".into())
}

pub fn status(app: &AppHandle) -> Result<Snapshot, String> {
    #[cfg(target_os = "macos")]
    {
        macos::status(app, &executable()?)
    }
    #[cfg(windows)]
    {
        windows::status(app, &executable()?)
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
        macos::set(app, &executable()?, enabled)
    }
    #[cfg(windows)]
    {
        windows::set(app, &executable()?, enabled)
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
