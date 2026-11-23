use crate::{defs, utils::{ensure_dir_exists, ensure_file_exists}};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}},
    path::{Path, PathBuf},
};

pub(super) struct InstallTransaction {
    directory: PathBuf,
    publication_guard: Option<MetamodulePublication>,
    metadata_guard: Option<File>,
    _lock: File,
    record: Value,
    committed: bool,
    commit_visible: bool,
    record_durable: bool,
}

fn root() -> PathBuf {
    Path::new(defs::WORKING_DIR).join(".module-install")
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_existing_parent(path: &Path) -> Result<()> {
    let mut parent = path.parent().context("Missing parent directory")?;
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => return sync_dir(parent),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                parent = parent.parent().context("Missing existing parent directory")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW)
                .open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(path)
}

fn remove_entry(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn private_root() -> Result<PathBuf> {
    let path = root();
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => sync_dir(Path::new(defs::WORKING_DIR))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(fs::symlink_metadata(&path)?.is_dir(), "Invalid installation state directory");
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn lock(module_id: &str, wait: bool) -> Result<Option<File>> {
    let held = std::env::var("KSUD_REMOVAL_CALLBACK_IDS").unwrap_or_default();
    let wait = wait && !held.split(':').any(|id| id == module_id);
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW)
        .open(private_root()?.join(format!(".lock-{module_id}")))?;
    loop {
        let flags = libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB };
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(Some(file));
        }
        let error = io::Error::last_os_error();
        if !wait && error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

pub(super) fn metadata_lock(module_id: &str) -> Result<File> {
    super::validate_module_id(module_id)?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW)
        .open(private_root()?.join(format!(".metadata-lock-{module_id}")))?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

pub(super) struct MetamodulePublication {
    _lock: File,
}

fn metamodule_path() -> &'static Path {
    Path::new(defs::METAMODULE_DIR.trim_end_matches('/'))
}

pub(super) fn validate_metamodule_state(state: &Value) -> Result<()> {
    if state.is_null() { return Ok(()); }
    ensure!(state["dev"].is_u64() && state["ino"].is_u64()
        && matches!(state["kind"].as_str(), Some("symlink" | "directory")),
        "Invalid metamodule resource identity");
    ensure!(if state["kind"] == "symlink" { state["target"].is_string() }
        else { state["target"].is_null() }, "Invalid metamodule resource target");
    Ok(())
}

pub(super) fn validate_metamodule_owner(owner: &Value) -> Result<()> {
    if owner.is_null() { return Ok(()); }
    ensure!(owner["version"] == 1 && owner["generation"].as_str().is_some_and(|value|
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "Invalid metamodule publication owner");
    ensure!(owner.as_object().is_some_and(|owner| owner.contains_key("state")),
        "Missing metamodule owner state");
    validate_metamodule_state(&owner["state"])
}

fn metamodule_state(path: &Path) -> Result<Value> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(error) => return Err(error.into()),
    };
    let (kind, target) = if metadata.file_type().is_symlink() {
        ("symlink", Value::String(fs::read_link(path)?.into_os_string().into_string()
            .map_err(|_| anyhow::anyhow!("Invalid metamodule link encoding"))?))
    } else {
        ensure!(metadata.is_dir(), "Invalid metamodule resource type");
        ("directory", Value::Null)
    };
    Ok(json!({"kind":kind,"target":target,"dev":metadata.dev(),"ino":metadata.ino()}))
}

impl MetamodulePublication {
    pub(super) fn lock() -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW)
            .open(private_root()?.join(".metamodule-lock"))?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self { _lock: file });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted { return Err(error.into()); }
        }
    }

    fn ensure_available(&self, module_id: &str) -> Result<()> {
        let current = self.current()?;
        let owner = self.owner()?;
        ensure!(owner.is_null() || owner["state"] == current,
            "Metamodule resource changed outside publication");
        if owner["state"]["kind"] == "symlink" {
            let target = Path::new(owner["state"]["target"].as_str().context("Missing metamodule owner target")?);
            let resolved = if target.is_absolute() { target.to_path_buf() } else {
                metamodule_path().parent().context("Missing metamodule parent")?.join(target)
            };
            if resolved != Path::new(defs::MODULE_DIR).join(module_id) {
                let existing_id = resolved.file_name().and_then(|id| id.to_str()).unwrap_or("unknown");
                return super::duplicate_metamodule_error(existing_id);
            }
        }
        super::ensure_unique_metamodule(module_id)
    }

    pub(super) fn current(&self) -> Result<Value> {
        metamodule_state(metamodule_path())
    }

    pub(super) fn owner(&self) -> Result<Value> {
        let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW)
            .open(root().join(".metamodule-owner.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Value::Null),
            Err(error) => return Err(error.into()),
        };
        let owner: Value = serde_json::from_reader(file)?;
        validate_metamodule_owner(&owner)?;
        Ok(owner)
    }

    pub(super) fn set_owner(&self, generation: &Value, state: &Value) -> Result<()> {
        let owner = json!({"version":1,"generation":generation,"state":state});
        validate_metamodule_owner(&owner)?;
        let temporary = root().join(".metamodule-owner.tmp");
        let mut file = OpenOptions::new().write(true).create(true).truncate(true)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        serde_json::to_writer(&mut file, &owner)?;
        file.sync_all()?;
        fs::rename(temporary, root().join(".metamodule-owner.json"))?;
        sync_dir(&root())
    }

    pub(super) fn unlink(&self, expected: &Value) -> Result<()> {
        ensure!(self.current()? == *expected, "Metamodule resource changed before removal");
        if !expected.is_null() { remove_entry(metamodule_path())?; }
        sync_dir(Path::new(defs::ADB_DIR))
    }
}

fn has_metamodule_publication(record: &Value) -> bool {
    !record["metamodulePublication"].is_null()
        || record["entries"].as_array().is_some_and(|entries| entries.iter().any(|entry|
            entry["name"] == "metamodule"))
}

fn restore_metamodule(directory: &Path, record: &Value, guard: &MetamodulePublication) -> Result<()> {
    let plan = &record["metamodulePublication"];
    let owner = guard.owner()?;
    if owner["generation"] != record["generation"] { return Ok(()); }
    let current = guard.current()?;
    let before = &plan["before"];
    let after = &plan["after"];
    let saved = directory.join("metamodule-backup");
    let backup = metamodule_state(&saved)?;
    if owner["state"] == *before {
        ensure!(current == *before, "Restored metamodule resource changed");
        return Ok(());
    }
    ensure!(owner["state"] == *after, "Metamodule publication owner changed");
    ensure!(current.is_null() || current == *before || current == *after,
        "Metamodule resource changed during installation rollback");
    if current == *before {
        ensure!(backup.is_null(), "Ambiguous metamodule rollback backup");
    } else {
        ensure!(backup == *before, "Metamodule rollback backup changed");
        if !current.is_null() { guard.unlink(after)?; }
        if !before.is_null() {
            fs::rename(&saved, metamodule_path())?;
            sync_dir(Path::new(defs::ADB_DIR))?;
            sync_dir(directory)?;
        }
    }
    sync_dir(Path::new(defs::ADB_DIR))?;
    sync_dir(directory)?;
    // Keep this generation even after restoration, so older recovery never revives it.
    guard.set_owner(&record["generation"], before)
}

fn restore_legacy_metamodule(directory: &Path, record: &Value, index: usize,
    entry: &Value, guard: &MetamodulePublication) -> Result<()>
{
    let saved = directory.join(format!("entry-{index}"));
    let backup = metamodule_state(&saved)?;
    let current = guard.current()?;
    let owner = guard.owner()?;
    if !owner.is_null() {
        if owner["generation"] != record["generation"] { return Ok(()); }
        if current == owner["state"] && backup.is_null() {
            sync_dir(Path::new(defs::ADB_DIR))?;
            return sync_dir(directory);
        }
        ensure!(current.is_null() && backup == owner["state"],
            "Legacy metamodule restoration changed");
    } else {
        ensure!(!backup.is_null() || entry["existed"] == false,
            "Legacy metamodule rollback target unconfirmed");
        ensure!(current.is_null(), "Legacy metamodule publication outcome unconfirmed");
        if backup.is_null() { guard.unlink(&Value::Null)?; }
        guard.set_owner(&record["generation"], &backup)?;
    }
    if !backup.is_null() {
        fs::rename(&saved, metamodule_path())?;
        sync_dir(Path::new(defs::ADB_DIR))?;
        sync_dir(directory)?;
    }
    Ok(())
}

fn metadata_path(module_id: &str, name: &str) -> Result<PathBuf> {
    match name {
        "module.prop" | "update" | "remove" | "disable" | ".ksu_hot_install" => {
            Ok(Path::new(defs::MODULE_DIR).join(module_id).join(name))
        }
        "metamodule" => Ok(PathBuf::from(defs::METAMODULE_DIR.trim_end_matches('/'))),
        "remove-intent" => Ok(root().join(format!(".remove-{module_id}.json"))),
        _ => bail!("Invalid installation metadata entry"),
    }
}

fn read_record(directory: &Path, module_id: &str) -> Result<Option<Value>> {
    let text = match fs::read(directory.join("record")) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: Value = serde_json::from_slice(&text)?;
    ensure!(record["version"] == 1 && record["id"] == module_id,
        "Invalid installation transaction identity");
    ensure!(record["generation"].as_str().is_some_and(|value|
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "Invalid installation transaction generation");
    ensure!(matches!(record["phase"].as_str(), Some("preparing" | "committed")),
        "Invalid installation transaction phase");
    ensure!(record["oldPending"].is_boolean() && record["createdLive"].is_boolean()
        && record["entries"].is_array(), "Invalid installation transaction state");
    for entry in record["entries"].as_array().context("Missing metadata intents")? {
        metadata_path(module_id, entry["name"].as_str().context("Missing entry name")?)?;
        ensure!(entry["existed"].is_boolean(), "Invalid metadata intent");
    }
    if !record["metamodulePublication"].is_null() {
        let plan = &record["metamodulePublication"];
        ensure!(plan.as_object().is_some_and(|plan|
            ["oldOwner", "before", "after"].iter().all(|key| plan.contains_key(*key))),
            "Invalid metamodule publication intent");
        validate_metamodule_owner(&plan["oldOwner"])?;
        validate_metamodule_state(&plan["before"])?;
        validate_metamodule_state(&plan["after"])?;
        ensure!(plan["after"]["kind"] == "symlink"
            && plan["after"]["target"].as_str().map(Path::new)
                == Some(Path::new(defs::MODULE_DIR).join(module_id).as_path()),
            "Invalid metamodule publication target");
    }
    Ok(Some(record))
}

fn restore(directory: &Path, record: &Value, publication: Option<&MetamodulePublication>) -> Result<()> {
    let module_id = record["id"].as_str().context("Missing module identity")?;
    let pending = Path::new(defs::MODULE_UPDATE_DIR).join(module_id);
    let old_pending = directory.join("pending");
    if fs::symlink_metadata(&old_pending).is_ok() {
        remove_entry(&pending)?;
        fs::rename(&old_pending, &pending)?;
        sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?;
        sync_dir(directory)?;
    } else if record["oldPending"] == false {
        remove_entry(&pending)?;
        sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?;
    }
    if !record["metamodulePublication"].is_null() {
        restore_metamodule(directory, record, publication.context("Missing metamodule publication guard")?)?;
    }
    let entries = record["entries"].as_array().context("Missing metadata intents")?;
    for (index, entry) in entries.iter().enumerate().rev() {
        if entry["name"] == "metamodule" {
            restore_legacy_metamodule(directory, record, index, entry,
                publication.context("Missing legacy metamodule publication guard")?)?;
            continue;
        }
        let path = metadata_path(module_id, entry["name"].as_str().context("Missing entry name")?)?;
        let saved = directory.join(format!("entry-{index}"));
        if fs::symlink_metadata(&saved).is_ok() {
            remove_entry(&path)?;
            fs::rename(&saved, &path)?;
        } else if entry["existed"] == false {
            remove_entry(&path)?;
        }
        sync_existing_parent(&path)?;
        sync_dir(directory)?;
    }
    if record["createdLive"] == true {
        match fs::remove_dir(Path::new(defs::MODULE_DIR).join(module_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        sync_dir(Path::new(defs::MODULE_DIR))?;
    }
    Ok(())
}

fn recover(directory: &Path, module_id: &str) -> Result<()> {
    if !directory.try_exists()? { return Ok(()); }
    let _metadata = metadata_lock(module_id)?;
    let record = read_record(directory, module_id)?;
    let rollback = record.as_ref().is_some_and(|record| record["phase"] != "committed");
    if rollback {
        let record = record.as_ref().context("Missing recovery record")?;
        let publication = if has_metamodule_publication(record) {
            Some(MetamodulePublication::lock()?)
        } else { None };
        restore(directory, record, publication.as_ref())?;
    }
    fs::remove_dir_all(directory)?;
    sync_dir(&root())?;
    if rollback {
        let _ = writeln!(io::stderr(), "- Failed installation of {module_id} rolled back");
    }
    Ok(())
}

pub(super) fn recover_before_boot() -> Result<()> {
    let entries = match fs::read_dir(root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let module_id = entry.file_name().to_string_lossy().into_owned();
        super::validate_module_id(&module_id)?;
        let Some(_lock) = lock(&module_id, false)? else {
            continue;
        };
        if let Err(error) = recover(&entry.path(), &module_id) {
            log::warn!("Module {module_id} installation recovery: {error:#}");
        }
    }
    Ok(())
}

pub(super) fn mutation_lock(module_id: &str, wait: bool) -> Result<Option<File>> {
    let Some(lock) = lock(module_id, wait)? else { return Ok(None); };
    recover(&root().join(module_id), module_id)?;
    Ok(Some(lock))
}

pub(super) fn skip_uncommitted(path: &Path, pending: bool) -> bool {
    let Some(module_id) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    match read_record(&root().join(module_id), module_id) {
        Ok(Some(record)) => record["phase"] != "committed"
            && (pending || record["createdLive"] == true
                || record["entries"].as_array().is_some_and(|entries| entries.iter().any(|entry|
                    entry["existed"] == true
                        && matches!(entry["name"].as_str(), Some("remove" | "remove-intent"))))),
        Ok(None) => false,
        Err(error) => {
            log::warn!("Module {module_id} installation state unreadable: {error:#}");
            true
        }
    }
}

impl InstallTransaction {
    pub(super) fn begin(module_id: &str) -> Result<Self> {
        let lock = lock(module_id, true)?.context("Missing installation lock")?;
        let directory = root().join(module_id);
        recover(&directory, module_id)?;
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let generation: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let old_pending = Path::new(defs::MODULE_UPDATE_DIR).join(module_id);
        let pending_existed = match fs::symlink_metadata(&old_pending) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        DirBuilder::new().mode(0o700).create(&directory)?;
        let mut transaction = Self {
            directory, _lock: lock, publication_guard: None, metadata_guard: None,
            committed: false, commit_visible: false, record_durable: false,
            record: json!({"version":1,"id":module_id,"generation":generation,
                "phase":"preparing","oldPending":pending_existed,"createdLive":false,"entries":[]}),
        };
        transaction.persist()?;
        sync_dir(&root())?;
        transaction.record_durable = true;
        if pending_existed {
            fs::rename(&old_pending, transaction.directory.join("pending"))?;
            sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?;
            sync_dir(&transaction.directory)?;
        }
        Ok(transaction)
    }

    fn persist(&mut self) -> Result<()> {
        let temporary = self.directory.join("record.tmp");
        let mut file = OpenOptions::new().write(true).create(true).truncate(true)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        serde_json::to_writer(&mut file, &self.record)?;
        file.sync_all()?;
        fs::rename(temporary, self.directory.join("record"))?;
        if self.record["phase"] == "committed" {
            self.commit_visible = true;
        }
        sync_dir(&self.directory)
    }

    pub(super) fn temporary_dir(&self) -> PathBuf {
        self.directory.join("tmp")
    }

    fn save_entry(&mut self, name: &str) -> Result<()> {
        let module_id = self.record["id"].as_str().context("Missing module identity")?;
        let path = metadata_path(module_id, name)?;
        let existed = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let entries = self.record["entries"].as_array_mut().context("Missing metadata intents")?;
        let index = entries.len();
        entries.push(json!({"name":name,"existed":existed}));
        self.persist()?;
        if existed {
            fs::rename(&path, self.directory.join(format!("entry-{index}")))?;
            sync_dir(path.parent().context("Missing metadata parent")?)?;
            sync_dir(&self.directory)?;
        }
        Ok(())
    }

    pub(super) fn publish_metadata(&mut self, is_metamodule: bool) -> Result<()> {
        let module_id = self.record["id"].as_str().context("Missing module identity")?.to_owned();
        if self.metadata_guard.is_none() {
            self.metadata_guard = Some(metadata_lock(&module_id)?);
        }
        if is_metamodule {
            sync_tree(&Path::new(defs::MODULE_UPDATE_DIR).join(&module_id))?;
            sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?;
            self.publication_guard = Some(MetamodulePublication::lock()?);
            self.publication_guard.as_ref().context("Missing metamodule publication guard")?
                .ensure_available(&module_id)?;
        }
        let live = Path::new(defs::MODULE_DIR).join(&module_id);
        self.record["createdLive"] = match fs::symlink_metadata(&live) {
            Ok(_) => false.into(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => true.into(),
            Err(error) => return Err(error.into()),
        };
        self.persist()?;
        ensure_dir_exists(&live)?;
        for name in ["module.prop", defs::UPDATE_FILE_NAME, defs::REMOVE_FILE_NAME,
            defs::DISABLE_FILE_NAME, defs::HOT_INSTALL_MARKER, "remove-intent"]
        {
            self.save_entry(name)?;
        }
        fs::copy(Path::new(defs::MODULE_UPDATE_DIR).join(&module_id).join("module.prop"),
            live.join("module.prop"))?;
        File::open(live.join("module.prop"))?.sync_all()?;
        ensure_file_exists(live.join(defs::UPDATE_FILE_NAME))?;
        if is_metamodule {
            let guard = self.publication_guard.as_ref().context("Missing metamodule publication guard")?;
            let before = guard.current()?;
            let old_owner = guard.owner()?;
            ensure!(old_owner.is_null() || old_owner["state"] == before,
                "Metamodule resource changed outside publication");
            let prepared = self.directory.join("metamodule-published");
            std::os::unix::fs::symlink(&live, &prepared)?;
            sync_dir(&self.directory)?;
            let after = metamodule_state(&prepared)?;
            self.record["metamodulePublication"] = json!({
                "oldOwner":old_owner,"before":before,"after":after});
            self.persist()?;
            let guard = self.publication_guard.as_ref().context("Missing metamodule publication guard")?;
            guard.set_owner(&self.record["generation"], &after)?;
            ensure!(guard.current()? == before, "Metamodule resource changed before publication");
            if !before.is_null() {
                fs::rename(metamodule_path(), self.directory.join("metamodule-backup"))?;
                sync_dir(Path::new(defs::ADB_DIR))?;
                sync_dir(&self.directory)?;
            }
            fs::rename(&prepared, metamodule_path())?;
            sync_dir(Path::new(defs::ADB_DIR))?;
            sync_dir(&self.directory)?;
        }
        sync_dir(&live)?;
        sync_dir(Path::new(defs::MODULE_DIR))?;
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<()> {
        if self.publication_guard.is_none() {
            sync_tree(&Path::new(defs::MODULE_UPDATE_DIR).join(
                self.record["id"].as_str().context("Missing module identity")?))?;
            sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?;
        }
        self.record["phase"] = "committed".into();
        self.persist()?;
        self.committed = true;
        drop(self.publication_guard.take());
        Ok(())
    }

    pub(super) fn take_metadata_guard(&mut self) -> Option<File> {
        self.metadata_guard.take()
    }
}

impl Drop for InstallTransaction {
    fn drop(&mut self) {
        if !self.record_durable {
            let _ = fs::remove_dir_all(&self.directory);
            return;
        }
        if self.commit_visible && !self.committed {
            let _ = writeln!(io::stderr(), "! Module commit receipt unconfirmed; retained {}", self.directory.display());
            return;
        }
        if !self.committed {
            let result = (|| -> Result<()> {
                if self.metadata_guard.is_none() {
                    let id = self.record["id"].as_str().context("Missing module identity")?;
                    self.metadata_guard = Some(metadata_lock(id)?);
                }
                if has_metamodule_publication(&self.record) && self.publication_guard.is_none() {
                    self.publication_guard = Some(MetamodulePublication::lock()?);
                }
                self.record["phase"] = "preparing".into();
                self.persist()?;
                restore(&self.directory, &self.record, self.publication_guard.as_ref())
            })();
            if let Err(error) = result {
                let _ = writeln!(io::stderr(), "! Module rollback incomplete: {error:#}; retained {}", self.directory.display());
                return;
            }
        }
        let cleanup = fs::remove_dir_all(&self.directory).map_err(anyhow::Error::from)
            .and_then(|()| sync_dir(&root()));
        match cleanup {
            Ok(()) if !self.committed => {
                let _ = writeln!(io::stderr(), "- Failed installation of {} rolled back", self.record["id"]);
            }
            Ok(()) => {}
            Err(error) => {
                let _ = writeln!(io::stderr(), "! Module transaction cleanup: {error:#}");
            }
        }
    }
}
