//! A registration is authoritative only after ownership, repair and target read-back.
use super::LoginState;

#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub present: bool,
    pub state: LoginState,
}

pub trait Entry {
    type Value: PartialEq;
    fn read(&mut self) -> Result<Option<Self::Value>, String>;
    fn retarget(&self, value: &Self::Value, executable: &str) -> Result<Self::Value, String>;
    fn write(&mut self, value: &Self::Value) -> Result<(), String>;
    fn state(&mut self, value: &Self::Value) -> Result<LoginState, String>;
}

pub fn repair<E: Entry>(entry: &mut E, executable: &str) -> Result<Option<E::Value>, String> {
    let Some(before) = entry.read()? else {
        return Ok(None);
    };
    let desired = entry.retarget(&before, executable)?;
    if before != desired {
        entry.write(&desired).map_err(|e| format!("Cannot migrate startup registration: {e}"))?;
    }
    let actual = entry.read()?.ok_or("Startup registration disappeared during migration")?;
    if actual != desired || entry.retarget(&actual, executable)? != actual {
        return Err("Startup registration target failed read-back validation".into());
    }
    Ok(Some(actual))
}

pub fn reconcile<E: Entry>(entry: &mut E, executable: &str) -> Result<Snapshot, String> {
    match repair(entry, executable)? {
        None => Ok(Snapshot { present: false, state: LoginState::Disabled }),
        Some(value) => Ok(Snapshot { present: true, state: entry.state(&value)? }),
    }
}

#[cfg(any(windows, test))]
pub fn windows_retarget(value: &str, executable: &str) -> Result<String, String> {
    if !windows_owned_path(executable) {
        return Err("Current startup executable is not a recognized Gwar executable".into());
    }
    let owned = if let Some(quoted) = value.strip_prefix('"') {
        quoted.trim_end_matches(' ').strip_suffix('"').is_some_and(windows_owned_path)
    } else {
        // The old plugin wrote exactly the executable path plus one space, with no args.
        // Other unquoted values can be another program followed by arguments.
        value == executable || value.strip_suffix(' ') == Some(executable)
    };
    if !owned {
        return Err("Startup entry is not a recognized Gwar executable; left unchanged".into());
    }
    super::windows_command(executable)
}

#[cfg(any(windows, test))]
fn windows_owned_path(path: &str) -> bool {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    let rest = if bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\" {
        &path[3..]
    } else if let Some(unc) = path.strip_prefix(r"\\") {
        // Both a server and a share are required for an absolute UNC file path.
        if unc.split('\\').count() < 3 {
            return false;
        }
        unc
    } else {
        return false;
    };
    !rest.chars().any(|c| c.is_control() || matches!(c, '"' | ':' | '/' | '<' | '>' | '|' | '?' | '*'))
        && rest.split('\\').all(|part| !part.is_empty() && part != "." && part != "..")
        && rest
            .rsplit('\\')
            .next()
            .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "gwar.exe" | "gwar-desktop.exe"))
}

#[cfg(any(target_os = "macos", test))]
const LEGACY_PREFIX: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
    "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    "<plist version=\"1.0\">\n  <dict>\n  <key>Label</key>\n  <string>Gwar</string>\n  ",
    "<key>AssociatedBundleIdentifiers</key>\n  <array></array>\n  <key>ProgramArguments</key>\n  <array><string>"
);
#[cfg(any(target_os = "macos", test))]
const LEGACY_SUFFIX: &str = "</string></array>\n  <key>RunAtLoad</key>\n  <true/>\n  </dict>\n</plist>";

#[cfg(any(target_os = "macos", test))]
fn macos_owned_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    path.is_absolute()
        && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
        && path.file_name().is_some_and(|name| name == "gwar" || name == "gwar-desktop")
        && path.parent().and_then(|p| p.file_name()).is_some_and(|name| name == "MacOS")
        && path.parent().and_then(|p| p.parent()).and_then(|p| p.file_name()).is_some_and(|name| name == "Contents")
        && path.ancestors().nth(3).and_then(|p| p.extension()).is_some_and(|ext| ext == "app")
}

#[cfg(any(target_os = "macos", test))]
pub fn macos_value(bytes: &[u8]) -> Result<plist::Value, String> {
    if bytes.len() > 64 * 1024 {
        return Err("Startup plist is too large; left unchanged".into());
    }
    if let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(bytes)) {
        return Ok(value);
    }
    // Only recover the exact former plugin template. Unknown malformed content is never discarded.
    let legacy = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let target = legacy
        .strip_prefix(LEGACY_PREFIX)
        .and_then(|s| s.strip_suffix(LEGACY_SUFFIX))
        .filter(|path| macos_owned_path(path) && !path.chars().any(char::is_control))
        .ok_or("Unrecognized startup plist; left unchanged")?;
    let mut value =
        plist::Value::from_reader_xml(super::launch_agent(target)?.as_slice()).map_err(|e| e.to_string())?;
    // Preserve the old empty association array as well as its RunAtLoad choice.
    value
        .as_dictionary_mut()
        .ok_or("Invalid legacy plist")?
        .insert("AssociatedBundleIdentifiers".into(), plist::Value::Array(Vec::new()));
    Ok(value)
}

#[cfg(any(target_os = "macos", test))]
pub fn macos_retarget(bytes: &[u8], executable: &str) -> Result<Vec<u8>, String> {
    let valid_xml = plist::Value::from_reader(std::io::Cursor::new(bytes)).is_ok();
    let mut value = macos_value(bytes)?;
    let dict = value.as_dictionary_mut().ok_or("Unrecognized startup plist; left unchanged")?;
    let identifiers = dict
        .get("AssociatedBundleIdentifiers")
        .and_then(plist::Value::as_array)
        .ok_or("Unrecognized startup plist ownership; left unchanged")?;
    if dict.get("Label").and_then(plist::Value::as_string) != Some("Gwar")
        || !identifiers.iter().all(|id| id.as_string() == Some("app.gwar.desktop"))
        || dict.contains_key("Program")
        || dict.contains_key("BundleProgram")
    {
        return Err("Startup entry is not a recognized Gwar plist; left unchanged".into());
    }
    let args = dict
        .get("ProgramArguments")
        .and_then(plist::Value::as_array)
        .ok_or("Unrecognized Gwar startup arguments; left unchanged")?;
    if args.len() != 1 || !args[0].as_string().is_some_and(macos_owned_path) || !macos_owned_path(executable) {
        return Err("Startup entry is not a recognized Gwar executable; left unchanged".into());
    }
    if dict.get("RunAtLoad").and_then(plist::Value::as_boolean).is_none()
        || dict.get("Disabled").is_some_and(|flag| flag.as_boolean().is_none())
    {
        return Err("Unrecognized startup enable flags; left unchanged".into());
    }
    if valid_xml && args[0].as_string() == Some(executable) {
        return Ok(bytes.to_vec());
    }
    dict.insert("ProgramArguments".into(), plist::Value::Array(vec![plist::Value::String(executable.into())]));
    let mut repaired = Vec::new();
    value.to_writer_xml(&mut repaired).map_err(|e| e.to_string())?;
    if plist::Value::from_reader_xml(repaired.as_slice()).map_err(|e| e.to_string())? != value {
        return Err("Cannot preserve startup plist; left unchanged".into());
    }
    Ok(repaired)
}

#[cfg(any(target_os = "macos", test))]
pub fn macos_disabled(bytes: &[u8]) -> Result<bool, String> {
    let value = macos_value(bytes)?;
    let dict = value.as_dictionary().ok_or("Invalid startup plist")?;
    Ok(dict.get("Disabled").and_then(plist::Value::as_boolean).unwrap_or(false)
        || dict.get("RunAtLoad").and_then(plist::Value::as_boolean) != Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory<V> {
        value: Option<V>,
        retarget: fn(&V, &str) -> Result<V, String>,
        state: LoginState,
        writes: usize,
        queries: usize,
        fail: bool,
        ignore: bool,
    }
    impl<V: Clone + PartialEq> Entry for Memory<V> {
        type Value = V;
        fn read(&mut self) -> Result<Option<V>, String> {
            Ok(self.value.clone())
        }
        fn retarget(&self, value: &V, exe: &str) -> Result<V, String> {
            (self.retarget)(value, exe)
        }
        fn write(&mut self, value: &V) -> Result<(), String> {
            self.writes += 1;
            if self.fail {
                return Err("migration denied".into());
            }
            if !self.ignore {
                self.value = Some(value.clone());
            }
            Ok(())
        }
        fn state(&mut self, _: &V) -> Result<LoginState, String> {
            self.queries += 1;
            Ok(self.state)
        }
    }
    fn win(value: Option<&str>, state: LoginState) -> Memory<String> {
        Memory {
            value: value.map(String::from),
            retarget: |v, e| windows_retarget(v, e),
            state,
            writes: 0,
            queries: 0,
            fail: false,
            ignore: false,
        }
    }
    fn mac(value: Option<Vec<u8>>, state: LoginState) -> Memory<Vec<u8>> {
        Memory {
            value,
            retarget: |v, e| macos_retarget(v, e),
            state,
            writes: 0,
            queries: 0,
            fail: false,
            ignore: false,
        }
    }
    const WIN: &str = r"C:\Program Files\New Gwar\gwar.exe";
    const MAC: &str = "/Applications/New & Friends/Gwar.app/Contents/MacOS/gwar";

    #[test]
    fn r4_s1_refuses_ambiguous_unquoted_commands_without_writes_or_state_queries() {
        for value in [
            r"C:\Tools\helper.exe \gwar.exe",
            r"C:\Tools\helper.exe \gwar.exe ",
            r"C:\Old\gwar.exe ",
            &format!("{WIN}  "),
        ] {
            let mut entry = win(Some(value), LoginState::Enabled);
            let error = reconcile(&mut entry, WIN).expect_err("ambiguous unquoted command is not ours");
            assert!(error.contains("not a recognized Gwar executable"));
            assert_eq!(entry.value.as_deref(), Some(value));
            assert_eq!((entry.writes, entry.queries), (0, 0));
        }
    }
    #[test]
    fn r3_s1_migrates_legacy_and_moved_windows_targets_without_enabling_disabled_entries() {
        for command in [format!("{WIN} "), r#""D:\Moved Gwar\gwar.exe""#.into(), WIN.into()] {
            for state in [LoginState::Enabled, LoginState::Disabled] {
                let mut entry = win(Some(&command), state);
                assert_eq!(reconcile(&mut entry, WIN).unwrap(), Snapshot { present: true, state });
                assert_eq!(entry.value.as_deref(), Some(format!("\"{WIN}\"").as_str()));
                assert_eq!(entry.writes, 1);
                reconcile(&mut entry, WIN).unwrap();
                assert_eq!(entry.writes, 1, "current registrations are not rewritten");
            }
        }
        let mut absent = win(None, LoginState::Enabled);
        assert_eq!(reconcile(&mut absent, WIN).unwrap().state, LoginState::Disabled);
        assert_eq!(absent.writes, 0);
        assert_eq!(absent.queries, 0);
    }
    #[test]
    fn recognizes_the_packaged_binary_name_as_well_as_legacy_gwar_names() {
        assert_eq!(
            windows_retarget(r#""C:\Old\gwar-desktop.exe""#, r"D:\New\gwar-desktop.exe").unwrap(),
            r#""D:\New\gwar-desktop.exe""#
        );
        let old = super::super::launch_agent("/Applications/Old/Gwar.app/Contents/MacOS/gwar-desktop").unwrap();
        let target = "/Applications/New/Gwar.app/Contents/MacOS/gwar-desktop";
        let mut entry = mac(Some(old), LoginState::Enabled);
        assert!(reconcile(&mut entry, target).unwrap().present);
        assert_eq!(macos_retarget(entry.value.as_ref().unwrap(), target).unwrap(), entry.value.unwrap());
    }
    #[test]
    fn r3_s1_refuses_foreign_commands_and_surfaces_failed_migration_or_stale_readback() {
        for value in [r#""C:\Windows\cmd.exe""#, r#""C:\Gwar\gwar.exe" --other"#, "anything", r"relative\gwar.exe"] {
            let mut entry = win(Some(value), LoginState::Enabled);
            assert!(reconcile(&mut entry, WIN).is_err());
            assert_eq!(entry.value.as_deref(), Some(value));
            assert_eq!((entry.writes, entry.queries), (0, 0));
        }
        for ignored in [false, true] {
            let mut entry = win(Some(r#""C:\Old\gwar.exe""#), LoginState::Enabled);
            entry.fail = !ignored;
            entry.ignore = ignored;
            assert!(reconcile(&mut entry, WIN).is_err());
            assert_eq!(entry.writes, 1, "exercise an owned entry's failed repair");
            assert_eq!(entry.queries, 0, "never report state after unsuccessful repair");
        }
    }
    #[test]
    fn r3_s2_recovers_exact_legacy_ampersand_xml_and_moves_it_before_authorization() {
        // Generated independently by the locked auto-launch 0.6.0 serializer.
        let legacy = include_bytes!("fixtures/legacy-macos-ampersand.plist").to_vec();
        assert!(plist::Value::from_reader_xml(legacy.as_slice()).is_err());
        let mut entry = mac(Some(legacy), LoginState::Enabled);
        assert_eq!(reconcile(&mut entry, MAC).unwrap().state, LoginState::Enabled);
        let value = plist::Value::from_reader_xml(entry.value.as_ref().unwrap().as_slice()).unwrap();
        assert_eq!(value.as_dictionary().unwrap()["ProgramArguments"].as_array().unwrap()[0].as_string(), Some(MAC));
        assert_eq!(value.as_dictionary().unwrap()["AssociatedBundleIdentifiers"].as_array().unwrap().len(), 0);
        assert_eq!((entry.writes, entry.queries), (1, 1));
    }
    #[test]
    fn r3_s2_preserves_flags_unknown_fields_and_refuses_foreign_or_failed_repairs() {
        let old = super::super::launch_agent("/Applications/Old/Gwar.app/Contents/MacOS/gwar").unwrap();
        let mut value = plist::Value::from_reader_xml(old.as_slice()).unwrap();
        let dict = value.as_dictionary_mut().unwrap();
        dict.insert("Disabled".into(), plist::Value::Boolean(true));
        dict.insert("RunAtLoad".into(), plist::Value::Boolean(false));
        dict.insert("FutureSetting".into(), plist::Value::String("retain".into()));
        let mut bytes = Vec::new();
        value.to_writer_xml(&mut bytes).unwrap();
        let mut entry = mac(Some(bytes.clone()), LoginState::Disabled);
        reconcile(&mut entry, MAC).unwrap();
        let after = plist::Value::from_reader_xml(entry.value.as_ref().unwrap().as_slice()).unwrap();
        let mut expected = value.clone();
        expected
            .as_dictionary_mut()
            .unwrap()
            .insert("ProgramArguments".into(), plist::Value::Array(vec![plist::Value::String(MAC.into())]));
        assert_eq!(after, expected);
        assert!(macos_disabled(entry.value.as_ref().unwrap()).unwrap());
        for ignored in [false, true] {
            let mut entry = mac(Some(bytes.clone()), LoginState::Enabled);
            entry.fail = !ignored;
            entry.ignore = ignored;
            assert!(reconcile(&mut entry, MAC).is_err());
            assert_eq!(entry.queries, 0);
        }
        let mut unknown = mac(Some(b"malformed & unknown".to_vec()), LoginState::Enabled);
        assert!(reconcile(&mut unknown, MAC).is_err());
        assert_eq!(unknown.writes, 0);
        value.as_dictionary_mut().unwrap().insert("Label".into(), plist::Value::String("Another app".into()));
        let mut foreign = Vec::new();
        value.to_writer_xml(&mut foreign).unwrap();
        let mut entry = mac(Some(foreign.clone()), LoginState::Enabled);
        assert!(reconcile(&mut entry, MAC).is_err());
        assert_eq!(entry.value, Some(foreign));
        assert_eq!(entry.writes, 0);
    }
}
