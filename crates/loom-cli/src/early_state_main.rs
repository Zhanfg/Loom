#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const DESIRED: &str = "desired";
const ATTEMPTED: &str = "attempted";
const CONFIRMED: &str = "confirmed";
const FAILED: &str = "failed";
const FORCE_STOCK: &str = "force-stock";

fn main() {
    if let Err(error) = run() {
        eprintln!("loom-early-state: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args();
    let _program = args.next();
    let command = args.next().ok_or("missing command")?;
    match command.as_str() {
        "arm" => {
            let state = required_path(&mut args, "state directory")?;
            let generation = required_generation(&mut args)?;
            reject_extra(args)?;
            arm(&state, &generation)?;
        }
        "decide" => {
            let state = required_path(&mut args, "state directory")?;
            let snapshots = required_path(&mut args, "snapshot directory")?;
            reject_extra(args)?;
            println!("{}", decide(&state, &snapshots)?);
        }
        "confirm" => {
            let state = required_path(&mut args, "state directory")?;
            let snapshots = required_path(&mut args, "snapshot directory")?;
            let generation = required_generation(&mut args)?;
            reject_extra(args)?;
            confirm(&state, &snapshots, &generation)?;
        }
        "force-stock" => {
            let state = required_path(&mut args, "state directory")?;
            let value = args.next().ok_or("missing force-stock value: on|off")?;
            reject_extra(args)?;
            force_stock(&state, &value)?;
        }
        "status" => {
            let state = required_path(&mut args, "state directory")?;
            reject_extra(args)?;
            print_status(&state)?;
        }
        _ => return Err(format!("unknown command: {command}").into()),
    }
    Ok(())
}

fn arm(state: &Path, generation: &str) -> Result<(), StateError> {
    ensure_state_dir(state)?;
    atomic_write(state, DESIRED, generation)?;
    remove_if_exists(&state.join(ATTEMPTED))?;
    remove_if_exists(&state.join(FAILED))?;
    sync_directory(state)?;
    Ok(())
}

fn decide(state: &Path, snapshots: &Path) -> Result<Decision, StateError> {
    ensure_state_dir(state)?;

    if state.join(FORCE_STOCK).exists() {
        return Ok(Decision::Stock("force-stock"));
    }

    let desired = read_generation_optional(&state.join(DESIRED))?;
    let confirmed = read_generation_optional(&state.join(CONFIRMED))?;
    let attempted = read_generation_optional(&state.join(ATTEMPTED))?;
    let failed = read_generation_optional(&state.join(FAILED))?;

    let Some(desired) = desired else {
        return Ok(Decision::Stock("no-desired-generation"));
    };

    if !snapshot_valid(snapshots, &desired)? {
        return Ok(fallback_decision(
            snapshots,
            confirmed.as_deref(),
            "desired-snapshot-invalid",
        )?);
    }

    if confirmed.as_deref() == Some(desired.as_str()) {
        return Ok(Decision::Confirmed(desired));
    }

    if failed.as_deref() == Some(desired.as_str()) {
        return Ok(fallback_decision(
            snapshots,
            confirmed.as_deref(),
            "candidate-quarantined",
        )?);
    }

    if attempted.as_deref() == Some(desired.as_str()) {
        // The exact candidate was marked attempted before the previous early handoff,
        // but userspace never confirmed it. Quarantine it before choosing a fallback.
        atomic_write(state, FAILED, &desired)?;
        remove_if_exists(&state.join(ATTEMPTED))?;
        sync_directory(state)?;
        return Ok(fallback_decision(
            snapshots,
            confirmed.as_deref(),
            "previous-attempt-unconfirmed",
        )?);
    }

    // This write is the one-shot safety boundary. A future first-stage host must
    // durably record the attempt before it redirects the system mount.
    atomic_write(state, ATTEMPTED, &desired)?;
    sync_directory(state)?;
    Ok(Decision::Candidate(desired))
}

fn confirm(state: &Path, snapshots: &Path, generation: &str) -> Result<(), StateError> {
    ensure_state_dir(state)?;
    if !snapshot_valid(snapshots, generation)? {
        return Err(StateError::InvalidSnapshot(generation.to_owned()));
    }
    let desired = read_generation_optional(&state.join(DESIRED))?;
    if desired.as_deref() != Some(generation) {
        return Err(StateError::ConfirmMismatch {
            desired,
            confirmed: generation.to_owned(),
        });
    }
    atomic_write(state, CONFIRMED, generation)?;
    remove_if_exists(&state.join(ATTEMPTED))?;
    remove_if_exists(&state.join(FAILED))?;
    sync_directory(state)?;
    Ok(())
}

fn force_stock(state: &Path, value: &str) -> Result<(), StateError> {
    ensure_state_dir(state)?;
    match value {
        "on" => atomic_write(state, FORCE_STOCK, "1")?,
        "off" => remove_if_exists(&state.join(FORCE_STOCK))?,
        _ => return Err(StateError::InvalidForceStockValue(value.to_owned())),
    }
    sync_directory(state)?;
    Ok(())
}

fn fallback_decision(
    snapshots: &Path,
    confirmed: Option<&str>,
    reason: &'static str,
) -> Result<Decision, StateError> {
    if let Some(generation) = confirmed {
        if snapshot_valid(snapshots, generation)? {
            return Ok(Decision::LastGood {
                generation: generation.to_owned(),
                reason,
            });
        }
    }
    Ok(Decision::Stock(reason))
}

fn snapshot_valid(root: &Path, generation: &str) -> Result<bool, StateError> {
    validate_generation(generation)?;
    let descriptor = root.join(generation).join("descriptor.env");
    if !descriptor.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(&descriptor).map_err(StateError::Io)?;
    let mut descriptor_generation = None;
    let mut state = None;
    let mut table_hash = None;
    let mut shadow_hash = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("LOOM_GENERATION=") {
            descriptor_generation = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("LOOM_STATE=") {
            state = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("LOOM_TABLE_SHA256=") {
            table_hash = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("LOOM_SHADOW_SHA256=") {
            shadow_hash = Some(value.to_owned());
        }
    }
    if descriptor_generation.as_deref() != Some(generation) {
        return Ok(false);
    }
    if !matches!(state.as_deref(), Some("PREPARED_NOT_ACTIVE") | Some("CONFIRMED")) {
        return Ok(false);
    }
    if !valid_sha256(table_hash.as_deref()) || !valid_sha256(shadow_hash.as_deref()) {
        return Ok(false);
    }
    Ok(root.join(generation).join("early.table").is_file()
        && root.join(generation).join("shadow.pack").is_file())
}

fn valid_sha256(value: Option<&str>) -> bool {
    value.is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn print_status(state: &Path) -> Result<(), StateError> {
    ensure_state_dir(state)?;
    for key in [DESIRED, ATTEMPTED, CONFIRMED, FAILED] {
        match read_generation_optional(&state.join(key))? {
            Some(value) => println!("{key}={value}"),
            None => println!("{key}="),
        }
    }
    println!("force_stock={}", u8::from(state.join(FORCE_STOCK).exists()));
    Ok(())
}

fn ensure_state_dir(state: &Path) -> Result<(), StateError> {
    fs::create_dir_all(state).map_err(StateError::Io)?;
    if !state.is_dir() {
        return Err(StateError::InvalidStateDirectory(state.to_path_buf()));
    }
    Ok(())
}

fn atomic_write(state: &Path, name: &str, value: &str) -> Result<(), StateError> {
    let final_path = state.join(name);
    let temporary = state.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(StateError::Io)?;
    file.write_all(value.as_bytes()).map_err(StateError::Io)?;
    file.write_all(b"\n").map_err(StateError::Io)?;
    file.sync_all().map_err(StateError::Io)?;
    drop(file);
    fs::rename(&temporary, &final_path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        StateError::Io(error)
    })?;
    sync_directory(state)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), StateError> {
    let directory = File::open(path).map_err(StateError::Io)?;
    directory.sync_all().map_err(StateError::Io)
}

fn remove_if_exists(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Io(error)),
    }
}

fn read_generation_optional(path: &Path) -> Result<Option<String>, StateError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Io(error)),
    };
    let mut value = String::new();
    file.read_to_string(&mut value).map_err(StateError::Io)?;
    let value = value.trim();
    validate_generation(value)?;
    Ok(Some(value.to_owned()))
}

fn validate_generation(value: &str) -> Result<(), StateError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(StateError::InvalidGeneration(value.to_owned()));
    }
    Ok(())
}

fn required_generation(args: &mut impl Iterator<Item = String>) -> Result<String, Box<dyn Error>> {
    let value = args.next().ok_or("missing generation")?;
    validate_generation(&value)?;
    Ok(value)
}

fn required_path(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}").into())
}

fn reject_extra(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = args.next() {
        return Err(format!("unexpected extra argument: {extra}").into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Stock(&'static str),
    Candidate(String),
    Confirmed(String),
    LastGood {
        generation: String,
        reason: &'static str,
    },
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stock(reason) => write!(f, "action=stock reason={reason}"),
            Self::Candidate(generation) => {
                write!(f, "action=candidate generation={generation} reason=first-attempt")
            }
            Self::Confirmed(generation) => {
                write!(f, "action=confirmed generation={generation} reason=last-good")
            }
            Self::LastGood { generation, reason } => {
                write!(f, "action=last-good generation={generation} reason={reason}")
            }
        }
    }
}

#[derive(Debug)]
enum StateError {
    Io(std::io::Error),
    InvalidGeneration(String),
    InvalidSnapshot(String),
    InvalidStateDirectory(PathBuf),
    InvalidForceStockValue(String),
    ConfirmMismatch {
        desired: Option<String>,
        confirmed: String,
    },
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "state I/O error: {error}"),
            Self::InvalidGeneration(value) => write!(f, "invalid generation id {value:?}"),
            Self::InvalidSnapshot(value) => write!(f, "invalid or missing snapshot for {value}"),
            Self::InvalidStateDirectory(path) => {
                write!(f, "invalid state directory {}", path.display())
            }
            Self::InvalidForceStockValue(value) => {
                write!(f, "invalid force-stock value {value:?}; expected on|off")
            }
            Self::ConfirmMismatch { desired, confirmed } => write!(
                f,
                "cannot confirm {confirmed}: desired generation is {}",
                desired.as_deref().unwrap_or("<none>")
            ),
        }
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("loom-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(root: &Path, generation: &str) {
        let dir = root.join(generation);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("shadow.pack"), b"shadow").unwrap();
        fs::write(dir.join("early.table"), b"table").unwrap();
        fs::write(
            dir.join("descriptor.env"),
            format!(
                "LOOM_GENERATION={generation}\nLOOM_STATE=PREPARED_NOT_ACTIVE\nLOOM_SHADOW_SHA256={}\nLOOM_TABLE_SHA256={}\n",
                "a".repeat(64),
                "b".repeat(64)
            ),
        )
        .unwrap();
    }

    #[test]
    fn first_attempt_is_one_shot_then_falls_back_to_stock() {
        let temp = TempDir::new("one-shot");
        let state = temp.0.join("state");
        let snapshots = temp.0.join("snapshots");
        snapshot(&snapshots, "g-a");
        arm(&state, "g-a").unwrap();
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::Candidate("g-a".into())
        );
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::Stock("previous-attempt-unconfirmed")
        );
        assert_eq!(read_generation_optional(&state.join(FAILED)).unwrap().as_deref(), Some("g-a"));
    }

    #[test]
    fn confirmed_generation_becomes_last_good_for_failed_upgrade() {
        let temp = TempDir::new("last-good");
        let state = temp.0.join("state");
        let snapshots = temp.0.join("snapshots");
        snapshot(&snapshots, "g-a");
        snapshot(&snapshots, "g-b");

        arm(&state, "g-a").unwrap();
        assert!(matches!(decide(&state, &snapshots).unwrap(), Decision::Candidate(_)));
        confirm(&state, &snapshots, "g-a").unwrap();
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::Confirmed("g-a".into())
        );

        arm(&state, "g-b").unwrap();
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::Candidate("g-b".into())
        );
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::LastGood {
                generation: "g-a".into(),
                reason: "previous-attempt-unconfirmed"
            }
        );
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::LastGood {
                generation: "g-a".into(),
                reason: "candidate-quarantined"
            }
        );
    }

    #[test]
    fn force_stock_always_wins() {
        let temp = TempDir::new("force-stock");
        let state = temp.0.join("state");
        let snapshots = temp.0.join("snapshots");
        snapshot(&snapshots, "g-a");
        arm(&state, "g-a").unwrap();
        force_stock(&state, "on").unwrap();
        assert_eq!(decide(&state, &snapshots).unwrap(), Decision::Stock("force-stock"));
        force_stock(&state, "off").unwrap();
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::Candidate("g-a".into())
        );
    }

    #[test]
    fn invalid_desired_snapshot_falls_back_to_confirmed() {
        let temp = TempDir::new("invalid-candidate");
        let state = temp.0.join("state");
        let snapshots = temp.0.join("snapshots");
        snapshot(&snapshots, "g-a");
        arm(&state, "g-a").unwrap();
        let _ = decide(&state, &snapshots).unwrap();
        confirm(&state, &snapshots, "g-a").unwrap();
        arm(&state, "g-missing").unwrap();
        assert_eq!(
            decide(&state, &snapshots).unwrap(),
            Decision::LastGood {
                generation: "g-a".into(),
                reason: "desired-snapshot-invalid"
            }
        );
    }
}
