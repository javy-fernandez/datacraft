//! Credential-free launchers referencing an existing saved connection.

use crate::i18n::tr;
use crate::ui::explorer::TableRef;
use anyhow::Context;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_PAYLOAD: usize = 65_536;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub connection: u64,
    pub database: String,
    pub schema: String,
    pub table: String,
}

impl From<&TableRef> for Target {
    fn from(t: &TableRef) -> Self {
        Self { connection: t.conn, database: t.db.clone(), schema: t.schema.clone(), table: t.table.clone() }
    }
}

fn validate(target: &Target) -> anyhow::Result<()> {
    anyhow::ensure!(target.connection != 0 && !target.database.is_empty() && !target.table.is_empty(), tr("Invalid table shortcut target"));
    anyhow::ensure!([&target.database, &target.schema, &target.table].iter().all(|v| !v.contains('\0')), tr("Invalid table shortcut target"));
    Ok(())
}

pub fn encode(target: &Target) -> anyhow::Result<String> {
    validate(target)?;
    let json = serde_json::to_vec(target)?;
    anyhow::ensure!(json.len() <= MAX_PAYLOAD, tr("Table shortcut target is too large"));
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    anyhow::ensure!(payload.len() <= MAX_PAYLOAD, tr("Table shortcut target is too large"));
    Ok(payload)
}

pub fn decode(payload: &str) -> anyhow::Result<Target> {
    anyhow::ensure!(payload.len() <= MAX_PAYLOAD, tr("Table shortcut target is too large"));
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).context(tr("Invalid table shortcut payload"))?;
    let target = serde_json::from_slice(&bytes).context(tr("Invalid table shortcut payload"))?;
    validate(&target)?;
    Ok(target)
}
pub fn extension() -> &'static str {
    if cfg!(windows) {
        "lnk"
    } else if cfg!(target_os = "macos") {
        "command"
    } else {
        "sh"
    }
}

struct Staging {
    directory: PathBuf,
    file: PathBuf,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// The caller confirms overwriting an existing destination before calling this function.
/// The complete launcher is staged beside the destination and atomically renamed into place.
pub fn create(path: &Path, target: &Target) -> anyhow::Result<()> {
    let executable = std::env::current_exe().context(tr("Could not find the DataCraft executable"))?;
    #[cfg(target_os = "macos")]
    let executable = executable.canonicalize().context(tr("Could not find the DataCraft executable"))?;
    create_for_executable(path, target, &executable, cfg!(target_os = "macos"))
}

fn validate_executable(executable: &Path, macos: bool) -> anyhow::Result<()> {
    // A translocated mount is temporary; do not guess a persistent original bundle location.
    anyhow::ensure!(
        !macos || !executable.components().any(|part| part.as_os_str() == "AppTranslocation"),
        tr("Cannot create a table shortcut from an App Translocation path. Move DataCraft to Applications and reopen it first.")
    );
    Ok(())
}

fn create_for_executable(path: &Path, target: &Target, executable: &Path, macos: bool) -> anyhow::Result<()> {
    let payload = encode(target)?;
    anyhow::ensure!(
        path.extension().and_then(|v| v.to_str()).is_some_and(|v| v.eq_ignore_ascii_case(extension())),
        tr("Invalid table shortcut file extension")
    );
    let destination = std::path::absolute(path).context(tr("Could not resolve the table shortcut path"))?;
    let parent = destination.parent().context(tr("Invalid table shortcut path"))?;
    validate_executable(executable, macos)?;
    let directory = parent.join(format!(".datacraft-shortcut-{}", uuid::Uuid::new_v4()));
    #[cfg(unix)]
    let staged = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&directory)
    };
    #[cfg(not(unix))]
    let staged = std::fs::create_dir(&directory);
    staged.context(tr("Could not stage the table shortcut"))?;
    let staging = Staging { file: directory.join(format!("table.{}", extension())), directory };
    write_native(&staging.file, executable, &payload)?;
    let file = std::fs::OpenOptions::new().write(true).open(&staging.file).context(tr("Could not read the staged table shortcut"))?;
    file.sync_all().context(tr("Could not save the table shortcut"))?;
    drop(file);
    std::fs::rename(&staging.file, destination).context(tr("Could not publish the table shortcut"))?;
    Ok(())
}

#[cfg(windows)]
fn write_native(path: &Path, executable: &Path, payload: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let executable = executable.to_str().context(tr("Executable path is not valid Unicode"))?;
    let arguments = format!("--open-table {payload}");
    anyhow::ensure!(executable.encode_utf16().count() + arguments.len() + 4 <= 32_767, tr("The table shortcut exceeds the Windows command-line limit"));
    // WScript.Shell stores its Save path through the Windows ANSI code page. Create the link
    // under a neutral temporary name, then let Rust copy it to the Unicode destination.
    let directory = std::env::temp_dir().join(format!("dc-shortcut-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).context(tr("Could not stage the Windows shortcut"))?;
    let native = Staging { file: directory.join("table.lnk"), directory };
    // All variable values travel as JSON data on stdin; none becomes PowerShell source.
    let request = serde_json::to_vec(&serde_json::json!({
        "path": native.file.to_str().context(tr("Shortcut path is not valid Unicode"))?,
        "executable": executable,
        "arguments": arguments,
        "directory": Path::new(executable).parent().and_then(|p| p.to_str()).context(tr("Invalid executable directory"))?,
        "path_error": tr("Windows Script Host cannot represent the temporary shortcut path"),
        "executable_error": tr("Windows Script Host cannot represent the executable path"),
        "arguments_error": tr("Windows Script Host cannot represent the shortcut arguments"),
        "saved_executable_error": tr("Windows Script Host changed the saved executable path"),
        "saved_arguments_error": tr("Windows Script Host changed the saved shortcut arguments"),
    }))?;
    let script = r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
[Console]::InputEncoding = New-Object System.Text.UTF8Encoding
$request = [Console]::In.ReadToEnd() | ConvertFrom-Json
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut([string]$request.path)
if (-not [string]::Equals($shortcut.FullName, [string]$request.path, [StringComparison]::OrdinalIgnoreCase)) { throw [string]$request.path_error }
$shortcut.TargetPath = [string]$request.executable
$shortcut.Arguments = [string]$request.arguments
$shortcut.WorkingDirectory = [string]$request.directory
if (-not [string]::Equals($shortcut.TargetPath, [string]$request.executable, [StringComparison]::OrdinalIgnoreCase)) { throw [string]$request.executable_error }
if ($shortcut.Arguments -cne [string]$request.arguments) { throw [string]$request.arguments_error }
$shortcut.Save()
$saved = $shell.CreateShortcut([string]$request.path)
if (-not [string]::Equals($saved.TargetPath, [string]$request.executable, [StringComparison]::OrdinalIgnoreCase)) { throw [string]$request.saved_executable_error }
if ($saved.Arguments -cne [string]$request.arguments) { throw [string]$request.saved_arguments_error }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
"#;
    let encoded_script = base64::engine::general_purpose::STANDARD.encode(script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").context(tr("Could not locate Windows PowerShell"))?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-OutputFormat", "Text", "-EncodedCommand", &encoded_script])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context(tr("Could not start Windows PowerShell to create the shortcut"))?;
    let input = child.stdin.take().context(tr("Could not write the shortcut request"))?.write_all(&request);
    let output = child.wait_with_output().context(tr("Could not finish creating the shortcut"))?;
    input.context(tr("Could not write the shortcut request"))?;
    anyhow::ensure!(output.status.success(), "{}: {}", tr("Could not create the Windows shortcut"), String::from_utf8_lossy(&output.stderr).trim());
    std::fs::copy(&native.file, path).context(tr("Could not stage the generated Windows shortcut"))?;
    Ok(())
}

#[cfg(unix)]
fn write_native(path: &Path, executable: &Path, payload: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let executable = executable.to_str().context(tr("Executable path is not valid Unicode"))?;
    let executable = executable.replace('\'', "'\\''");
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(path).context(tr("Could not create the table launcher"))?;
    write!(file, "#!/bin/sh\nexec '{executable}' --open-table '{payload}'\n").context(tr("Could not write the table launcher"))?;
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn write_native(_path: &Path, _executable: &Path, _payload: &str) -> anyhow::Result<()> {
    anyhow::bail!(tr("Table shortcuts are not available on this platform"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_launcher_roundtrips_unicode_without_credentials_or_shell_syntax() {
        let t = Target { connection: 7, database: "shop".into(), schema: "a\"' schema".into(), table: "x; $(echo pwned) 雪".into() };
        let payload = encode(&t).unwrap();
        assert!(payload.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)));
        assert_eq!(decode(&payload).unwrap(), t);
        assert!(!serde_json::to_string(&t).unwrap().contains("password"));
    }

    #[test]
    fn invalid_or_oversized_launcher_targets_are_rejected() {
        assert!(decode("garbage!; open").is_err());
        assert!(decode(&"x".repeat(65537)).is_err());
        let t = Target { connection: 0, database: "".into(), schema: "".into(), table: "".into() };
        assert!(encode(&t).is_err());
        let unknown =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"connection":1,"database":"db","schema":"s","table":"t","password":"secret"}"#);
        assert!(decode(&unknown).is_err());
        let nul = Target { connection: 1, database: "db".into(), schema: "s\0".into(), table: "t".into() };
        assert!(encode(&nul).is_err());
        let oversized = Target { connection: 1, database: "db".into(), schema: "s".into(), table: "x".repeat(65_536) };
        assert!(encode(&oversized).is_err());
    }

    #[test]
    fn translocated_executable_is_refused_before_staging_or_replacing_a_launcher() {
        let executable = Path::new("/private/var/folders/example/T/AppTranslocation/1234/d/DataCraft.app/Contents/MacOS/DataCraft");
        assert!(validate_executable(executable, true).is_err());
        let root = std::env::temp_dir().join(format!("dc-translocated-shortcut-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join(format!("table.{}", extension()));
        std::fs::write(&path, b"preserve launcher").unwrap();
        let target = Target { connection: 7, database: "main".into(), schema: "main".into(), table: "notes".into() };
        let error = create_for_executable(&path, &target, executable, true).unwrap_err().to_string();
        assert!(error.contains("App Translocation"));
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve launcher");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn stable_executable_paths_remain_supported() {
        for executable in
            ["/Applications/DataCraft.app/Contents/MacOS/DataCraft", "/Users/me/Applications/AppTranslocation Helper.app/Contents/MacOS/DataCraft"]
        {
            assert!(validate_executable(Path::new(executable), true).is_ok());
        }
        assert!(validate_executable(Path::new("C:/Tools/AppTranslocation/DataCraft.exe"), false).is_ok());
    }

    #[test]
    fn creates_a_native_launcher_without_interpreting_path_or_table_syntax() {
        let root = std::env::temp_dir().join(format!("dc-launcher-'$(echo INJECTED); 雪-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join(format!("table.{}", extension()));
        std::fs::write(&path, b"previous launcher").unwrap();
        let target = Target { connection: 7, database: "shop".into(), schema: "public".into(), table: "t; $(echo INJECTED) ' 雪".into() };
        create(&path, &target).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let payload = encode(&target).unwrap();
        #[cfg(windows)]
        {
            assert_eq!(bytes.get(..20), Some(&[0x4c, 0, 0, 0, 1, 0x14, 2, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46][..]));
            let expected: Vec<u8> = format!("--open-table {payload}").encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert!(bytes.windows(expected.len()).any(|w| w == expected));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(String::from_utf8(bytes).unwrap().contains(&format!("--open-table '{payload}'")));
            assert_ne!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o100, 0);
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn failed_shortcut_validation_preserves_existing_destination() {
        let root = std::env::temp_dir().join(format!("dc-launcher-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join(format!("table.{}", extension()));
        std::fs::write(&path, b"preserve me").unwrap();
        let target = Target { connection: 1, database: "db".into(), schema: "".into(), table: "invalid\0name".into() };
        assert!(create(&path, &target).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve me");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn failed_publication_removes_staging_and_preserves_destination() {
        let root = std::env::temp_dir().join(format!("dc-launcher-publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let destination = root.join(format!("table.{}", extension()));
        std::fs::create_dir(&destination).unwrap();
        let sentinel = destination.join("preserve.txt");
        std::fs::write(&sentinel, b"preserve me").unwrap();
        let target = Target { connection: 1, database: "db".into(), schema: "s".into(), table: "t".into() };
        assert!(create(&destination, &target).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve me");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(&sentinel).unwrap();
        std::fs::remove_dir(&destination).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shell_launcher_passes_payload_as_one_argument_even_with_shell_syntax_in_executable() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("dc-shell-launcher-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let executable = root.join("app'$(touch INJECTED)'");
        std::fs::write(&executable, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launcher = root.join("launcher.sh");
        write_native(&launcher, &executable, "safe_payload-0123").unwrap();
        let output = std::process::Command::new(&launcher).current_dir(&root).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"--open-table\nsafe_payload-0123\n");
        assert!(!root.join("INJECTED").exists());
        std::fs::remove_file(&launcher).unwrap();
        std::fs::remove_file(&executable).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
