// SPDX-License-Identifier: AGPL-3.0-only
//! Where a local master key set is kept: a key file, or the system keychain through
//! `/usr/bin/security` (macOS) or `secret-tool` (Linux). There is no keychain crate.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aws_lc_rs::digest::{SHA256, digest};
use zeroize::Zeroizing;

use crate::key::KeyError;
use crate::local::MasterKeySet;

/// A key file larger than this is not a key set (one key is about 70 bytes of JSON).
const MAX_KEY_FILE: u64 = 1 << 20;
/// The service name of the keychain item.
pub const KEYCHAIN_SERVICE: &str = "knoggin";
/// `security find-generic-password` exits with this code when no item matches.
const SECURITY_NOT_FOUND: i32 = 44;

/// Errors of the key file and the keychain.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The key file or the keychain item does not exist.
    #[error("no master key is stored there")]
    NotFound,
    /// The key file is not a regular file, belongs to another user, or has permission bits it must not have.
    #[error("the key file {path} is not private to this user")]
    Permissions {
        /// The key file.
        path: PathBuf,
    },
    /// The path to write already exists, or is a link.
    #[error("{path} already exists")]
    Exists {
        /// The path that was refused.
        path: PathBuf,
    },
    /// The stored text is not a valid key set.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// Reading or writing the key file failed.
    #[error("key file: {0}")]
    Io(#[source] io::Error),
    /// The keychain tool is not installed.
    #[error("{0} is not installed")]
    ToolMissing(&'static str),
    /// The keychain tool failed. Its output is not kept: it could echo the secret.
    #[error("{tool} failed with exit code {code:?}")]
    ToolFailed {
        /// The program.
        tool: &'static str,
        /// Its exit code, if it exited.
        code: Option<i32>,
    },
    /// The keychain tool did not finish in time and was killed.
    #[error("{0} did not finish in time")]
    ToolTimedOut(&'static str),
    /// A write reported success but reading the item back did not give the same key set.
    #[error("the keychain did not keep the key set")]
    NotKept,
}

/// The keychain item's account for a Knoggin home folder: `master-key-` and the first 8 hex
/// characters of the SHA-256 of the home's absolute path. The caller passes an absolute path.
pub fn account_name(home: &Path) -> String {
    let hash = digest(&SHA256, home.as_os_str().as_encoded_bytes());
    hash.as_ref()[..4]
        .iter()
        .fold(String::from("master-key-"), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Which permission bits a key file may not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRule {
    /// The local daemon: owned by this user, no permission bit for group or others.
    Private,
    /// A server role reading a mounted file: not writable by group or others.
    NotWritableByOthers,
}

/// Reads a key set file, checking its owner and mode first.
pub fn read_key_file(path: &Path, rule: FileRule) -> Result<MasterKeySet, SourceError> {
    // A link is followed: the checks below apply to the file it names (`fstat` of the open file).
    let file = File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => SourceError::NotFound,
        _ => SourceError::Io(e),
    })?;
    let meta = file.metadata().map_err(SourceError::Io)?;
    let mode = meta.mode();
    let allowed = match rule {
        FileRule::Private => mode & 0o077 == 0 && meta.uid() == rustix::process::geteuid().as_raw(),
        FileRule::NotWritableByOthers => mode & 0o022 == 0,
    };
    if !meta.is_file() || !allowed {
        return Err(SourceError::Permissions { path: path.into() });
    }
    let mut text = Zeroizing::new(Vec::with_capacity(
        usize::try_from(meta.len().min(MAX_KEY_FILE)).unwrap_or(0) + 1,
    ));
    file.take(MAX_KEY_FILE + 1)
        .read_to_end(&mut text)
        .map_err(SourceError::Io)?;
    if text.len() as u64 > MAX_KEY_FILE {
        return Err(KeyError::InvalidKeySet("the key file is too large").into());
    }
    Ok(MasterKeySet::from_json(&text)?)
}

/// Creates a new key file with mode 0600. Refuses a path that exists or is a link.
pub fn create_key_file(path: &Path, keys: &MasterKeySet) -> Result<(), SourceError> {
    write_new(path, keys)?;
    sync_dir(path)
}

/// Replaces a key file: writes a temporary file beside it, syncs it and renames it over the old one.
pub fn replace_key_file(path: &Path, keys: &MasterKeySet) -> Result<(), SourceError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    // A temporary file left by a crashed run of the same PID would block `create_new`.
    let _ = fs::remove_file(&tmp);
    write_new(&tmp, keys)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SourceError::Io(e));
    }
    sync_dir(path)
}

fn write_new(path: &Path, keys: &MasterKeySet) -> Result<(), SourceError> {
    // `create_new` is O_CREAT|O_EXCL, which also refuses a symbolic link, dangling or not.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => SourceError::Exists { path: path.into() },
            _ => SourceError::Io(e),
        })?;
    let json = keys.to_json();
    file.write_all(json.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(SourceError::Io)
}

fn sync_dir(path: &Path) -> Result<(), SourceError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(SourceError::Io)
}

/// How long a keychain tool may take before it is killed. A locked keychain can prompt.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// What a finished tool returned. `stdout` wipes itself: it can hold the key set.
struct ToolOutput {
    code: Option<i32>,
    stdout: Zeroizing<Vec<u8>>,
    wrote_stderr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    MacSecurity,
    SecretTool,
}

/// The system keychain item that holds this home's master key set.
#[derive(Debug, Clone)]
pub struct Keychain {
    tool: Tool,
    account: String,
    program: PathBuf,
    timeout: Duration,
}

impl Keychain {
    /// The macOS login keychain, through `program` (in production `/usr/bin/security`).
    pub fn macos(account: String, program: PathBuf) -> Self {
        Self {
            tool: Tool::MacSecurity,
            account,
            program,
            timeout: TOOL_TIMEOUT,
        }
    }

    /// The Linux Secret Service, through `program` (in production `secret-tool`, found on `PATH`).
    pub fn secret_service(account: String, program: PathBuf) -> Self {
        Self {
            tool: Tool::SecretTool,
            account,
            program,
            timeout: TOOL_TIMEOUT,
        }
    }

    fn tool_name(&self) -> &'static str {
        match self.tool {
            Tool::MacSecurity => "security",
            Tool::SecretTool => "secret-tool",
        }
    }

    /// Runs the tool with `args`, writes `stdin` to it, and waits at most `self.timeout`.
    /// Blocking: callers run it inside `spawn_blocking`.
    fn run(&self, args: &[&str], stdin: &[u8]) -> Result<ToolOutput, SourceError> {
        let tool = self.tool_name();
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => SourceError::ToolMissing(tool),
                _ => SourceError::Io(e),
            })?;
        // The input is a few hundred bytes, well under a pipe's buffer, so this cannot block.
        let wrote = child
            .stdin
            .take()
            .map_or(Ok(()), |mut i| i.write_all(stdin));
        let deadline = Instant::now() + self.timeout;
        // ponytail: output is read after exit; a tool that prints more than a pipe buffer
        // (64 KiB) would stall until the timeout. Ours print one key set.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SourceError::ToolTimedOut(tool));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SourceError::Io(e));
                }
            }
        };
        wrote.map_err(SourceError::Io)?;
        let mut stdout = Zeroizing::new(Vec::new());
        if let Some(mut out) = child.stdout.take() {
            out.read_to_end(&mut stdout).map_err(SourceError::Io)?;
        }
        let mut stderr = Vec::new();
        if let Some(mut err) = child.stderr.take() {
            err.read_to_end(&mut stderr).map_err(SourceError::Io)?;
        }
        Ok(ToolOutput {
            code: status.code(),
            stdout,
            wrote_stderr: !stderr.is_empty(),
        })
    }

    /// Reads the key set. `NotFound` when the item does not exist.
    pub fn read(&self) -> Result<MasterKeySet, SourceError> {
        let account = self.account.as_str();
        let out = match self.tool {
            // A plain command: the secret is in no argument.
            Tool::MacSecurity => self.run(
                &[
                    "find-generic-password",
                    "-s",
                    KEYCHAIN_SERVICE,
                    "-a",
                    account,
                    "-w",
                ],
                b"",
            )?,
            Tool::SecretTool => self.run(
                &["lookup", "service", KEYCHAIN_SERVICE, "account", account],
                b"",
            )?,
        };
        let not_found = match self.tool {
            Tool::MacSecurity => out.code == Some(SECURITY_NOT_FOUND),
            // secret-tool exits 1 and prints nothing when no item matches; a real failure says why.
            Tool::SecretTool => out.code == Some(1) && out.stdout.is_empty() && !out.wrote_stderr,
        };
        if not_found {
            return Err(SourceError::NotFound);
        }
        if out.code != Some(0) {
            return Err(SourceError::ToolFailed {
                tool: self.tool_name(),
                code: out.code,
            });
        }
        Ok(MasterKeySet::from_keychain_text(&out.stdout)?)
    }

    /// Saves the key set, replacing the item, then reads it back to prove that it was kept.
    ///
    /// The read-back matters on macOS: `security -i` exits 0 even when its command failed.
    pub fn write(&self, keys: &MasterKeySet) -> Result<(), SourceError> {
        let text = keys.to_keychain_text();
        let out = match self.tool {
            Tool::MacSecurity => {
                // Written to `security -i` on standard input, so the secret is in no argument list.
                let mut cmd = Zeroizing::new(String::with_capacity(text.len() + 128));
                let _ = writeln!(
                    cmd,
                    "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {} -w {}",
                    self.account, *text
                );
                self.run(&["-i"], cmd.as_bytes())?
            }
            Tool::SecretTool => self.run(
                &[
                    "store",
                    "--label=Knoggin master key",
                    "service",
                    KEYCHAIN_SERVICE,
                    "account",
                    &self.account,
                ],
                text.as_bytes(),
            )?,
        };
        if out.code != Some(0) {
            return Err(SourceError::ToolFailed {
                tool: self.tool_name(),
                code: out.code,
            });
        }
        match self.read() {
            Ok(back) if back.to_json() == keys.to_json() => Ok(()),
            Ok(_) | Err(SourceError::NotFound) => Err(SourceError::NotKept),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn account_name_rule() {
        let name = account_name(Path::new("/Users/someone/.knoggin"));
        let hash = digest(&SHA256, b"/Users/someone/.knoggin");
        let hex: String = hash.as_ref()[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(name, format!("master-key-{hex}"));
        assert_eq!(name.len(), "master-key-".len() + 8);
        assert_ne!(name, account_name(Path::new("/Users/other/.knoggin")));
    }

    #[test]
    fn key_file_create_read_and_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        let keys = MasterKeySet::generate().unwrap();

        assert!(matches!(
            read_key_file(&path, FileRule::Private),
            Err(SourceError::NotFound)
        ));
        create_key_file(&path, &keys).unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        let back = read_key_file(&path, FileRule::Private).unwrap();
        assert_eq!(back.current_kek_id(), keys.current_kek_id());

        // Never overwrites.
        assert!(matches!(
            create_key_file(&path, &keys),
            Err(SourceError::Exists { .. })
        ));

        // Group or other bits: refused locally; the server rule refuses only write bits.
        for (mode, local_ok, server_ok) in [
            (0o640, false, true),
            (0o604, false, true),
            (0o620, false, false),
            (0o602, false, false),
            (0o400, true, true),
        ] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_key_file(&path, FileRule::Private).is_ok(),
                local_ok,
                "{mode:o}"
            );
            assert_eq!(
                read_key_file(&path, FileRule::NotWritableByOthers).is_ok(),
                server_ok,
                "{mode:o}"
            );
        }
    }

    #[test]
    fn key_file_links_and_bad_content() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.key");
        create_key_file(&real, &MasterKeySet::generate().unwrap()).unwrap();
        // Reading follows a link and checks the file it names.
        let link = dir.path().join("link.key");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(read_key_file(&link, FileRule::Private).is_ok());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_key_file(&link, FileRule::Private),
            Err(SourceError::Permissions { .. })
        ));
        // A link, even a dangling one, is never written through.
        let dangling = dir.path().join("dangling.key");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        assert!(matches!(
            create_key_file(&dangling, &MasterKeySet::generate().unwrap()),
            Err(SourceError::Exists { .. })
        ));
        assert!(!dir.path().join("nowhere").exists());

        let junk = dir.path().join("junk.key");
        fs::write(&junk, b"{\"version\":1}").unwrap();
        fs::set_permissions(&junk, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            read_key_file(&junk, FileRule::Private),
            Err(SourceError::Key(KeyError::InvalidKeySet(_)))
        ));

        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(read_key_file(&folder, FileRule::Private).is_err());
    }

    #[test]
    fn key_file_replace_for_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        let mut keys = MasterKeySet::generate().unwrap();
        create_key_file(&path, &keys).unwrap();
        keys.rotate().unwrap();
        replace_key_file(&path, &keys).unwrap();
        let back = read_key_file(&path, FileRule::Private).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.current_kek_id(), keys.current_kek_id());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A fake keychain tool: records each call's arguments (one per line) and standard input,
    /// keeps one item in a file, and answers as `security` or `secret-tool` would.
    const FAKE: &str = r#"#!/bin/sh
d=$(dirname "$0")
n=$(cat "$d/n" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$d/n"
for a in "$@"; do printf '%s\n' "$a"; done > "$d/args.$n"
cat > "$d/stdin.$n"
case "$1" in
  -i) [ -e "$d/drop_writes" ] || sed 's/.* //' "$d/stdin.$n" | tr -d '\n' > "$d/item"; exit 0 ;;
  store) [ -e "$d/drop_writes" ] || cp "$d/stdin.$n" "$d/item"; exit 0 ;;
  find-generic-password)
    [ -e "$d/fail_reads" ] && { echo locked >&2; exit 51; }
    [ -e "$d/item" ] || { echo 'could not be found' >&2; exit 44; }
    cat "$d/item"; echo; exit 0 ;;
  lookup)
    [ -e "$d/fail_reads" ] && { echo 'no D-Bus' >&2; exit 1; }
    [ -e "$d/item" ] || exit 1
    cat "$d/item"; exit 0 ;;
esac
exit 99
"#;

    const ACCOUNT: &str = "master-key-0123abcd";

    /// A temp folder holding the fake tool, and the two keychains that use it.
    fn fake(script: &str) -> (tempfile::TempDir, [Keychain; 2]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let kcs = [
            Keychain::macos(ACCOUNT.into(), path.clone()),
            Keychain::secret_service(ACCOUNT.into(), path),
        ];
        (dir, kcs)
    }

    /// Every recorded call: (arguments, standard input).
    fn calls(dir: &Path) -> Vec<(Vec<String>, Vec<u8>)> {
        let n: usize = fs::read_to_string(dir.join("n")).map_or(0, |s| s.trim().parse().unwrap());
        (1..=n)
            .map(|i| {
                let args = fs::read_to_string(dir.join(format!("args.{i}"))).unwrap();
                let args = args.lines().map(str::to_string).collect();
                (args, fs::read(dir.join(format!("stdin.{i}"))).unwrap())
            })
            .collect()
    }

    #[test]
    fn keychain_write_then_read() {
        for i in 0..2 {
            let (dir, kcs) = fake(FAKE);
            let kc = &kcs[i];
            assert!(matches!(kc.read(), Err(SourceError::NotFound)));
            let keys = MasterKeySet::generate().unwrap();
            kc.write(&keys).unwrap();
            assert_eq!(kc.read().unwrap().current_kek_id(), keys.current_kek_id());

            let text = keys.to_keychain_text();
            let calls = calls(dir.path());
            // The secret never appears in an argument list.
            for (args, _) in &calls {
                assert!(args.iter().all(|a| !a.contains(&*text)), "{args:?}");
            }
            let write = calls
                .iter()
                .find(|(a, _)| a[0] == "-i" || a[0] == "store")
                .unwrap();
            if i == 0 {
                assert_eq!(write.0, ["-i"]);
                assert_eq!(
                    String::from_utf8(write.1.clone()).unwrap(),
                    format!(
                        "add-generic-password -U -s knoggin -a master-key-0123abcd -w {}\n",
                        *text
                    )
                );
                let read = calls
                    .iter()
                    .find(|(a, _)| a[0] == "find-generic-password")
                    .unwrap();
                assert_eq!(
                    read.0,
                    [
                        "find-generic-password",
                        "-s",
                        "knoggin",
                        "-a",
                        "master-key-0123abcd",
                        "-w"
                    ]
                );
            } else {
                assert_eq!(
                    write.0,
                    [
                        "store",
                        "--label=Knoggin master key",
                        "service",
                        "knoggin",
                        "account",
                        "master-key-0123abcd"
                    ]
                );
                assert_eq!(write.1, text.as_bytes());
                let read = calls.iter().find(|(a, _)| a[0] == "lookup").unwrap();
                assert_eq!(
                    read.0,
                    [
                        "lookup",
                        "service",
                        "knoggin",
                        "account",
                        "master-key-0123abcd"
                    ]
                );
            }
        }
    }

    #[test]
    fn keychain_failures() {
        for i in 0..2 {
            let (dir, kcs) = fake(FAKE);
            fs::write(dir.path().join("drop_writes"), b"").unwrap();
            assert!(matches!(
                kcs[i].write(&MasterKeySet::generate().unwrap()),
                Err(SourceError::NotKept)
            ));

            let missing = [
                Keychain::macos(ACCOUNT.into(), dir.path().join("absent")),
                Keychain::secret_service(ACCOUNT.into(), dir.path().join("absent")),
            ];
            assert!(matches!(
                missing[i].read(),
                Err(SourceError::ToolMissing(_))
            ));

            let (dir, kcs) = fake(FAKE);
            fs::write(dir.path().join("fail_reads"), b"").unwrap();
            assert!(matches!(kcs[i].read(), Err(SourceError::ToolFailed { .. })));

            let (dir, kcs) = fake(FAKE);
            fs::write(dir.path().join("item"), b"bm90IGEga2V5IHNldA==").unwrap();
            assert!(matches!(
                kcs[i].read(),
                Err(SourceError::Key(KeyError::InvalidKeySet(_)))
            ));
        }
    }

    #[test]
    fn a_tool_that_hangs_is_killed() {
        let (_dir, kcs) = fake("#!/bin/sh\nexec sleep 30\n");
        for mut kc in kcs {
            kc.timeout = Duration::from_millis(200);
            let started = Instant::now();
            assert!(matches!(kc.read(), Err(SourceError::ToolTimedOut(_))));
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }
}
