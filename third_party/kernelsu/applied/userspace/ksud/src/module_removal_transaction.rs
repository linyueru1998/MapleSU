use crate::{defs, metamodule, utils::ensure_file_exists};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    Path::new(defs::WORKING_DIR).join(".module-install")
}

fn intent_path(module_id: &str) -> PathBuf {
    root().join(format!(".remove-{module_id}.json"))
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn identity(path: &Path) -> Result<Option<[u64; 2]>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some([metadata.dev(), metadata.ino()])),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36 && value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) { byte == b'-' } else { byte.is_ascii_hexdigit() }
    })
}

fn callback_boot_id() -> Result<String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    ensure!(valid_boot_id(value), "Invalid removal callback boot identity");
    Ok(value.to_owned())
}

fn read_intent(module_id: &str) -> Result<Option<Value>> {
    let bytes = match fs::read(intent_path(module_id)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: Value = serde_json::from_slice(&bytes)?;
    ensure!(record["version"] == 1 && record["id"] == module_id,
        "Invalid removal transaction identity");
    ensure!(record["generation"].as_str().is_some_and(|value|
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "Invalid removal generation");
    for key in ["live", "pending"] {
        if !record[key].is_null() {
            let parts = record[key].as_array().context("Invalid removal target identity")?;
            ensure!(parts.len() == 2 && parts.iter().all(Value::is_u64),
                "Invalid removal target identity");
        }
    }
    ensure!(record["metamodule"].is_boolean(), "Invalid metamodule removal state");
    ensure!(record["properties"].as_object().is_some_and(|properties|
        properties.values().all(Value::is_string)), "Invalid removal module metadata");
    for key in ["metaCallback", "ownCallback"] {
        ensure!(matches!(record[key].as_str(), Some("pending" | "running" | "done")),
            "Invalid removal callback phase");
        for suffix in ["Boot", "InterruptedBoot"] {
            let value = &record[format!("{key}{suffix}")];
            ensure!(value.is_null() || value.as_str().is_some_and(valid_boot_id),
                "Invalid removal callback boot identity");
        }
    }
    if !record["metamoduleRemoval"].is_null() {
        let plan = &record["metamoduleRemoval"];
        ensure!(plan.as_object().is_some_and(|plan|
            plan.contains_key("before") && plan.contains_key("owner")),
            "Invalid metamodule removal intent");
        super::install_transaction::validate_metamodule_state(&plan["before"])?;
        super::install_transaction::validate_metamodule_owner(&plan["owner"])?;
    }
    Ok(Some(record))
}

fn matches_identity(record: &Value, current: Option<[u64; 2]>) -> bool {
    current.is_none() || *record == json!(current)
}

pub(super) fn is_removed(module: &Path) -> bool {
    if module.join(defs::REMOVE_FILE_NAME).exists() { return true; }
    let Some(id) = module.file_name().and_then(|name| name.to_str()) else { return false; };
    match read_intent(id) {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            log::warn!("Module {id} removal state unreadable: {error:#}");
            true
        }
    }
}

pub(super) fn pending_ids() -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    let entries = match fs::read_dir(root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue; };
        let Some(id) = name.strip_prefix(".remove-").and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if let Err(error) = super::validate_module_id(id) {
            log::warn!("Invalid removal intent name {name}: {error:#}");
            continue;
        }
        ids.insert(id.to_owned());
    }
    Ok(ids)
}

pub(super) fn append_pending_rows(rows: &mut Vec<HashMap<String, String>>) -> Result<()> {
    for id in pending_ids()? {
        if rows.iter().any(|row| row.get("id") == Some(&id)) { continue; }
        let record = match read_intent(&id) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                log::warn!("Module {id} pending removal metadata unavailable: {error:#}");
                continue;
            }
        };
        let current = match identity(&Path::new(defs::MODULE_DIR).join(&id)) {
            Ok(current) => current,
            Err(error) => {
                log::warn!("Module {id} pending removal target unavailable: {error:#}");
                continue;
            }
        };
        if !matches_identity(&record["live"], current) {
            continue;
        }
        let mut row: HashMap<String, String> = serde_json::from_value(record["properties"].clone())?;
        row.insert("id".into(), id);
        for (key, value) in [("remove", "true"), ("enabled", "false"), ("update", "false"),
            ("web", "false"), ("action", "false"), ("mount", "false")]
        {
            row.insert(key.into(), value.into());
        }
        rows.push(row);
    }
    Ok(())
}

pub(super) fn mark(module_id: &str) -> Result<()> {
    let mut removal = Removal::open(module_id, true)?.context("Missing removal transaction")?;
    removal.check_targets()?;
    removal.persist()?;
    let live = Path::new(defs::MODULE_DIR).join(module_id);
    let marker = live.join(defs::REMOVE_FILE_NAME);
    ensure_file_exists(&marker)?;
    File::open(marker)?.sync_all()?;
    sync_dir(&live)
}

pub(super) fn cancel(module_id: &str) -> Result<()> {
    let live = Path::new(defs::MODULE_DIR).join(module_id);
    if let Some(record) = read_intent(module_id)? {
        ensure!(matches_identity(&record["live"], identity(&live)?),
            "Removal target changed for module {module_id}");
        ensure!(record["metaCallback"] == "pending" && record["ownCallback"] == "pending",
            "Removal already started for module {module_id}: metaCallback={} ownCallback={}",
            record["metaCallback"], record["ownCallback"]);
    }
    match fs::remove_file(live.join(defs::REMOVE_FILE_NAME)) {
        Ok(()) => sync_dir(&live)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::remove_file(intent_path(module_id)) {
        Ok(()) => sync_dir(&root())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) struct Removal {
    record: Value,
}

impl Removal {
    pub(super) fn open(module_id: &str, create: bool) -> Result<Option<Self>> {
        if let Some(record) = read_intent(module_id)? { return Ok(Some(Self { record })); }
        let live = Path::new(defs::MODULE_DIR).join(module_id);
        if !create && !live.join(defs::REMOVE_FILE_NAME).exists() { return Ok(None); }
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let generation: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let properties = super::read_module_prop(&live).unwrap_or_default();
        let mut removal = Self { record: json!({
            "version":1,"id":module_id,"generation":generation,
            "live":identity(&live)?,
            "pending":identity(&Path::new(defs::MODULE_UPDATE_DIR).join(module_id))?,
            "metamodule":metamodule::is_metamodule(&properties),
            "properties":properties,
            "metaCallback":"pending","ownCallback":"pending"
        }) };
        removal.persist()?;
        Ok(Some(removal))
    }

    fn id(&self) -> Result<&str> {
        self.record["id"].as_str().context("Missing removal module identity")
    }

    fn persist(&mut self) -> Result<()> {
        let id = self.id()?;
        if let Some(current) = read_intent(id)? {
            ensure!(current["generation"] == self.record["generation"],
                "Removal generation changed for module {id}");
        }
        let temporary = root().join(format!(".remove-{id}.tmp"));
        let mut file = OpenOptions::new().write(true).create(true).truncate(true)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        serde_json::to_writer(&mut file, &self.record)?;
        file.sync_all()?;
        fs::rename(temporary, intent_path(id))?;
        sync_dir(&root())
    }

    fn check_targets(&self) -> Result<()> {
        let id = self.id()?;
        for (key, directory) in [("live", defs::MODULE_DIR), ("pending", defs::MODULE_UPDATE_DIR)] {
            ensure!(matches_identity(&self.record[key], identity(&Path::new(directory).join(id))?),
                "Removal {key} target changed for module {id}");
        }
        Ok(())
    }

    fn callback(&mut self, phase: &str, run: impl FnOnce() -> Result<()>) -> Result<()> {
        match self.record[phase].as_str() {
            Some("done") => return Ok(()),
            Some("pending" | "running") => {}
            _ => bail!("Invalid removal callback phase"),
        }
        let boot = callback_boot_id()?;
        let boot_key = format!("{phase}Boot");
        if self.record[phase] == "running" {
            if let Some(previous) = self.record[&boot_key].as_str() {
                ensure!(previous != boot,
                    "Removal callback outcome unconfirmed for module {}: {phase}", self.id()?);
                let previous = previous.to_owned();
                log::warn!("Module {} removal callback interrupted in boot {previous}: {phase}; retrying", self.id()?);
                self.record[format!("{phase}InterruptedBoot")] = previous.into();
            } else {
                // A legacy running record needs an observed boot boundary before replay.
                self.record[&boot_key] = boot.into();
                self.persist()?;
                bail!("Removal callback outcome unconfirmed for module {}: {phase}", self.id()?);
            }
        }
        self.record[phase] = "running".into();
        self.record[&boot_key] = boot.into();
        self.persist()?;
        if let Err(error) = run() {
            log::warn!("Module {} removal callback: {error:#}", self.id()?);
        }
        self.record[phase] = "done".into();
        self.persist()
    }

    pub(super) fn run_callbacks(&mut self) -> Result<()> {
        self.check_targets()?;
        let id = self.id()?.to_owned();
        let is_meta = self.record["metamodule"] == true;
        self.callback("metaCallback", || {
            if is_meta { Ok(()) } else { metamodule::exec_metauninstall_script(&id) }
        })?;
        let uninstaller = Path::new(defs::MODULE_DIR).join(&id).join("uninstall.sh");
        self.callback("ownCallback", || {
            if uninstaller.exists() {
                super::exec_script_with(uninstaller, super::ScriptWait::Forever, |command| {
                    command.env("KSUD_REMOVAL_CALLBACK_IDS", super::removal_callback_ancestry(&id));
                })
            } else { Ok(()) }
        })
    }

    fn remove_metamodule(&mut self, live: &Path) -> Result<()> {
        if self.record["metamodule"] != true { return Ok(()); }
        let guard = super::install_transaction::MetamodulePublication::lock()?;
        let current = guard.current()?;
        let owner = guard.owner()?;
        let generation = self.record["generation"].clone();
        let existing = &self.record["metamoduleRemoval"];
        if !existing.is_null() {
            if owner["generation"] == generation {
                ensure!(owner["state"].is_null(), "Metamodule removal owner changed");
                ensure!(current.is_null() || current == existing["before"],
                    "Metamodule resource changed during removal");
                return guard.unlink(&current);
            }
            if owner != existing["owner"] { return Ok(()); }
            ensure!(current == existing["before"], "Metamodule removal target changed");
        } else {
            if !current.is_null() {
                if current["kind"] != "symlink" { return Ok(()); }
                let target = Path::new(current["target"].as_str().context("Missing metamodule target")?);
                let resolved = if target.is_absolute() { target.to_path_buf() } else {
                    Path::new(defs::METAMODULE_DIR.trim_end_matches('/')).parent()
                        .context("Missing metamodule parent")?.join(target)
                };
                if resolved != live { return Ok(()); }
                ensure!(owner.is_null() || owner["state"] == current,
                    "Metamodule resource changed outside publication");
            } else if !owner["state"].is_null() {
                if owner["state"]["kind"] != "symlink" { return Ok(()); }
                let target = Path::new(owner["state"]["target"].as_str()
                    .context("Missing metamodule owner target")?);
                let resolved = if target.is_absolute() { target.to_path_buf() } else {
                    Path::new(defs::METAMODULE_DIR.trim_end_matches('/')).parent()
                        .context("Missing metamodule parent")?.join(target)
                };
                if resolved != live { return Ok(()); }
            }
            self.record["metamoduleRemoval"] = json!({"before":current,"owner":owner});
            self.persist()?;
        }
        guard.set_owner(&generation, &Value::Null)?;
        guard.unlink(&current)
    }

    pub(super) fn remove(mut self) -> Result<()> {
        self.check_targets()?;
        let id = self.id()?.to_owned();
        let live = Path::new(defs::MODULE_DIR).join(&id);
        let pending = Path::new(defs::MODULE_UPDATE_DIR).join(&id);
        match fs::remove_dir_all(&pending) {
            Ok(()) => sync_dir(Path::new(defs::MODULE_UPDATE_DIR))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to remove pending module"),
        }
        let config_result = crate::module_config::clear_module_configs(&id).and_then(|()| {
            let directory = Path::new(defs::MODULE_CONFIG_DIR);
            match fs::metadata(directory) {
                Ok(_) => sync_dir(directory),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        });
        let mut first_error = config_result.err();
        if let Some(error) = &first_error {
            log::warn!("Module {id} config cleanup: {error:#}");
        }
        let meta_result = self.remove_metamodule(&live);
        if let Err(error) = meta_result {
            log::warn!("Module {id} metamodule cleanup: {error:#}");
            if first_error.is_none() { first_error = Some(error); }
        }
        let live_result = (|| -> Result<()> {
            match fs::remove_dir_all(&live) {
                Ok(()) => sync_dir(Path::new(defs::MODULE_DIR))?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(())
        })();
        if let Err(error) = live_result {
            log::warn!("Failed to remove live module {id}: {error:#}");
            if first_error.is_none() { first_error = Some(error); }
        }
        if let Some(error) = first_error { return Err(error); }
        let current = read_intent(&id)?.context("Removal intent disappeared before completion")?;
        ensure!(current["generation"] == self.record["generation"],
            "Removal generation changed before completion for module {id}");
        fs::remove_file(intent_path(&id))?;
        sync_dir(&root())?;
        log::info!("Module {id} removed");
        Ok(())
    }
}
