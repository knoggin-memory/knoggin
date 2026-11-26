// SPDX-License-Identifier: AGPL-3.0-only
//! Where a local master key set is kept: a key file, or the system keychain through
//! `/usr/bin/security` (macOS) or `secret-tool` (Linux). There is no keychain crate.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use aws_lc_rs::digest::{SHA256, digest};
use zeroize::Zeroizing;

use crate::key::KeyError;
use crate::local::MasterKeySet;

/// A key file larger than this is not a key set (one key is about 70 bytes of JSON).
const MAX_KEY_FILE: u64 = 1 << 20;
/// The service name of the keychain item.
pub const KEYCHAIN_SERVICE: &str = "knoggin";
const SECURITY: &str = "/usr/bin/security";
const SECRET_TOOL: &str = "secret-tool";
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

/// What a finished command returned. `stdout` wipes itself: it can hold the key set.
#[derive(Default)]
pub struct CommandOutput {
    /// Exit code, `None` if a signal ended it.
    pub code: Option<i32>,
    /// Standard output.
    pub stdout: Zeroizing<Vec<u8>>,
    /// Whether anything was written to standard error. The text itself is not kept.
    pub wrote_stderr: bool,
}

impl fmt::Debug for CommandOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandOutput")
            .field("code", &self.code)
            .field("stdout_len", &self.stdout.len())
            .field("wrote_stderr", &self.wrote_stderr)
            .finish()
    }
}

/// Runs the keychain tools. Tests replace it so that they never touch a real keychain.
pub trait CommandRunner: Send + Sync + fmt::Debug {
    /// Runs `program` with `args`, writes `stdin` to it, waits, and returns what it printed.
    fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> io::Result<CommandOutput>;
}

/// The real runner: spawns the process.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> io::Result<CommandOutput> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut input) = child.stdin.take() {
            input.write_all(stdin)?;
        }
        let out = child.wait_with_output()?;
        Ok(CommandOutput {
            code: out.status.code(),
            stdout: Zeroizing::new(out.stdout),
            wrote_stderr: !out.stderr.is_empty(),
        })
    }
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
    runner: Arc<dyn CommandRunner>,
}

impl Keychain {
    /// The macOS login keychain, through `/usr/bin/security`.
    pub fn macos(account: String, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            tool: Tool::MacSecurity,
            account,
            runner,
        }
    }

    /// The Linux Secret Service, through `secret-tool`.
    pub fn secret_service(account: String, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            tool: Tool::SecretTool,
            account,
            runner,
        }
    }

    fn tool_name(&self) -> &'static str {
        match self.tool {
            Tool::MacSecurity => SECURITY,
            Tool::SecretTool => SECRET_TOOL,
        }
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Result<CommandOutput, SourceError> {
        let program = self.tool_name();
        self.runner
            .run(program, args, stdin)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => SourceError::ToolMissing(program),
                _ => SourceError::Io(e),
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
    use std::sync::Mutex;

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

    type Call = (String, Vec<String>, Vec<u8>);

    /// A pretend keychain: one item, and a record of every call (program, arguments, stdin).
    #[derive(Debug, Default)]
    struct FakeRunner {
        item: Mutex<Option<Vec<u8>>>,
        calls: Mutex<Vec<Call>>,
        /// Ignore writes but still exit 0, as `security -i` does when its command fails.
        drop_writes: bool,
        missing: bool,
        fail_reads: bool,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str], stdin: &[u8]) -> io::Result<CommandOutput> {
            if self.missing {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            self.calls.lock().unwrap().push((
                program.into(),
                args.iter().map(|a| a.to_string()).collect(),
                stdin.to_vec(),
            ));
            let mut item = self.item.lock().unwrap();
            let ok = |stdout: Vec<u8>| CommandOutput {
                code: Some(0),
                stdout: Zeroizing::new(stdout),
                wrote_stderr: false,
            };
            match (program, args.first().copied()) {
                ("/usr/bin/security", Some("-i")) => {
                    let line = std::str::from_utf8(stdin).unwrap();
                    let secret = line.trim_end().rsplit(' ').next().unwrap();
                    if !self.drop_writes {
                        *item = Some(secret.as_bytes().to_vec());
                    }
                    Ok(ok(Vec::new()))
                }
                ("/usr/bin/security", Some("find-generic-password")) => match &*item {
                    _ if self.fail_reads => Ok(CommandOutput {
                        code: Some(51),
                        wrote_stderr: true,
                        ..CommandOutput::default()
                    }),
                    Some(v) => Ok(ok([v.as_slice(), b"\n"].concat())),
                    None => Ok(CommandOutput {
                        code: Some(44),
                        wrote_stderr: true,
                        ..CommandOutput::default()
                    }),
                },
                ("secret-tool", Some("store")) => {
                    if !self.drop_writes {
                        *item = Some(stdin.to_vec());
                    }
                    Ok(ok(Vec::new()))
                }
                ("secret-tool", Some("lookup")) => match &*item {
                    _ if self.fail_reads => Ok(CommandOutput {
                        code: Some(1),
                        wrote_stderr: true,
                        ..CommandOutput::default()
                    }),
                    Some(v) => Ok(ok(v.clone())),
                    None => Ok(CommandOutput {
                        code: Some(1),
                        ..CommandOutput::default()
                    }),
                },
                _ => panic!("unexpected call {program} {args:?}"),
            }
        }
    }

    fn both(runner: &Arc<FakeRunner>) -> [Keychain; 2] {
        let r: Arc<dyn CommandRunner> = runner.clone();
        [
            Keychain::macos("master-key-0123abcd".into(), r.clone()),
            Keychain::secret_service("master-key-0123abcd".into(), r),
        ]
    }

    #[test]
    fn keychain_write_then_read() {
        for i in 0..2 {
            let runner = Arc::new(FakeRunner::default());
            let kc = both(&runner)[i].clone();
            assert!(matches!(kc.read(), Err(SourceError::NotFound)));
            let keys = MasterKeySet::generate().unwrap();
            kc.write(&keys).unwrap();
            assert_eq!(kc.read().unwrap().current_kek_id(), keys.current_kek_id());

            let text = keys.to_keychain_text();
            let calls = runner.calls.lock().unwrap();
            // The secret never appears in an argument list.
            for (_, args, _) in calls.iter() {
                assert!(args.iter().all(|a| !a.contains(&*text)), "{args:?}");
            }
            let write = calls
                .iter()
                .find(|(_, a, _)| a[0] == "-i" || a[0] == "store")
                .unwrap();
            if i == 0 {
                assert_eq!(write.0, "/usr/bin/security");
                assert_eq!(
                    String::from_utf8(write.2.clone()).unwrap(),
                    format!(
                        "add-generic-password -U -s knoggin -a master-key-0123abcd -w {}\n",
                        *text
                    )
                );
                let read = calls
                    .iter()
                    .find(|(_, a, _)| a[0] == "find-generic-password")
                    .unwrap();
                assert_eq!(
                    read.1,
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
                assert_eq!(write.0, "secret-tool");
                assert_eq!(
                    write.1,
                    [
                        "store",
                        "--label=Knoggin master key",
                        "service",
                        "knoggin",
                        "account",
                        "master-key-0123abcd"
                    ]
                );
                assert_eq!(write.2, text.as_bytes());
                let read = calls.iter().find(|(_, a, _)| a[0] == "lookup").unwrap();
                assert_eq!(
                    read.1,
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
            let runner = Arc::new(FakeRunner {
                drop_writes: true,
                ..FakeRunner::default()
            });
            let kc = both(&runner)[i].clone();
            assert!(matches!(
                kc.write(&MasterKeySet::generate().unwrap()),
                Err(SourceError::NotKept)
            ));

            let runner = Arc::new(FakeRunner {
                missing: true,
                ..FakeRunner::default()
            });
            assert!(matches!(
                both(&runner)[i].read(),
                Err(SourceError::ToolMissing(_))
            ));

            let runner = Arc::new(FakeRunner {
                fail_reads: true,
                ..FakeRunner::default()
            });
            assert!(matches!(
                both(&runner)[i].read(),
                Err(SourceError::ToolFailed { .. })
            ));

            let runner = Arc::new(FakeRunner::default());
            *runner.item.lock().unwrap() = Some(b"bm90IGEga2V5IHNldA==".to_vec());
            assert!(matches!(
                both(&runner)[i].read(),
                Err(SourceError::Key(KeyError::InvalidKeySet(_)))
            ));
        }
    }

    #[test]
    fn system_runner_pipes_stdin_and_reports_codes() {
        let out = SystemRunner
            .run("/bin/sh", &["-c", "cat; exit 3"], b"hi")
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(&out.stdout[..], b"hi");
        assert!(!out.wrote_stderr);
        let err = SystemRunner
            .run("/nonexistent/knoggin-tool", &[], b"")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
