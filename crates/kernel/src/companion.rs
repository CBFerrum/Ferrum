//! Files for the programs that run beside the server (docs/operations.md, "Files for the programs beside the server";
//! docs/plan/stateless.md): a meeting server, a mail server, an engine of an app's own, the object store Compose bundles.
//! Each is declared by its owner as a *service*: an app inside its registration, the platform outside any
//! ([`crate::HookRegistry::add_companion`]). A service names the files it writes and the secrets the vault makes for it,
//! and has a function that is given its owner's vault handle and the resolved settings and returns the files.
//! `ferrum task render service:<name> to:<dir>` writes them as the program starts (an init container, a one-shot
//! service), from the database and the vault alone: a secret the server and the program share is made once, by the
//! vault, and typed by nobody.
//!
//! Nothing is written before everything is known: a closed vault, a setting the service needs and nobody set, a secret
//! its rule keeps from files, a file the service did not declare, each refuses the render before [`write`] is called.

use crate::Kernel;
use crate::error::{Error, Result};
use crate::vault::{Described, Rule, Vault, Zeroizing};
use async_trait::async_trait;
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

/// A program beside the server whose files its owner renders.
#[async_trait]
pub trait Companion: Send + Sync {
    /// The files it writes, by name: what `ferrum task services` lists, and exactly what a render returns.
    fn files(&self) -> &'static [&'static str];

    /// The secrets of its owner the vault keeps for it, at the installation's scope: made at its first render
    /// ([`Rendering::made`]), or kept from what was there ([`Rendering::keep`]); `ferrum task vault_rotate level:service`
    /// makes each of them anew.
    fn made(&self) -> &'static [&'static str] {
        &[]
    }

    /// Why this installation may not have the service rendered, in plain words: asked before every render and every
    /// rotation, which are then refused with nothing written and nothing made, and by `ferrum task services`, which says
    /// it. `None` (the default): it may.
    async fn refusal(&self, r: &Rendering<'_>) -> Result<Option<String>> {
        let _ = r;
        Ok(None)
    }

    /// Whether the files' group reads them too (mode 0640), for a program that runs as another user of that group;
    /// otherwise their owner alone does (0600).
    fn group_reads(&self) -> bool {
        false
    }

    /// The files' bytes. Whatever it cannot give (a setting nobody set, a secret its rule keeps from files) is an error,
    /// and then nothing is written.
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>>;
}

/// One file a service writes.
pub struct File {
    /// One of the names the service declares.
    pub name: String,
    pub bytes: Zeroizing<Vec<u8>>,
    /// Whether it holds a secret. One that says it does not and holds a value the vault gave this render is refused.
    pub secret: bool,
}

impl File {
    pub fn new(name: &str, bytes: Vec<u8>, secret: bool) -> File {
        File { name: name.to_string(), bytes: Zeroizing::new(bytes), secret }
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "File({}, {} bytes{})", self.name, self.bytes.len(), if self.secret { ", a secret" } else { "" })
    }
}

/// A version of a secret given to a render: the current one, or the one before it while its overlap lasts.
pub struct Version {
    pub version: i32,
    pub value: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Version({})", self.version)
    }
}

/// What a service's function is given: its owner's vault handle, the settings as the server resolved them, and the
/// secrets the render may write into files. Made by the kernel alone, for one render.
pub struct Rendering<'a> {
    kernel: &'a Kernel,
    vault: Vault,
    service: &'a str,
    made: &'static [&'static str],
    settings: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// The secrets' values given to this render: no file that says it holds no secret may hold one.
    given: Mutex<Vec<Zeroizing<Vec<u8>>>>,
    /// A rotation's render ([`Kernel::rotate_companion`]): what it made anew. Its files are not written, and it is given
    /// no secret's value.
    rotated: Option<Mutex<Vec<Described>>>,
}

impl Rendering<'_> {
    /// The service being rendered.
    pub fn service(&self) -> &str {
        self.service
    }

    pub fn kernel(&self) -> &Kernel {
        self.kernel
    }

    /// The service's owner's handle: its own keys and secrets, and nobody else's.
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// A setting's value, by its key or its variable, as the server resolved it (the environment's through this
    /// release, else the database's); `None` where nothing sets it to something.
    pub fn setting(&self, name: &str) -> Option<String> {
        (self.settings)(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// A setting the service cannot do without: refused, naming it, when nothing sets it.
    pub fn needs(&self, name: &str) -> Result<String> {
        self.setting(name).ok_or_else(|| {
            Error::Invalid(format!("the service {} needs the setting {name}, which is not set: ferrum task set key:{name} value:<value>", self.service))
        })
    }

    /// Makes `name`, one of the secrets the service declares ([`Companion::made`]), where its owner has none at the
    /// installation's scope: a random value of 32 bytes as text (43 characters of base64url) under `rule`, marked as its
    /// owner's to manage, so that people's routes leave it to the app. Renders and replicas at once make one. A secret of
    /// that name that a person set is refused: the vault makes it here only where there is none.
    pub async fn made(&self, name: &str, rule: Rule) -> Result<()> {
        if !self.made.contains(&name) {
            return Err(Error::Invalid(format!("the service {} does not declare that the vault makes {name} for it", self.service)));
        }
        if self.rotated.is_some() {
            return self.renew(name, rule).await;
        }
        self.vault.make_for(self.kernel, name, rule, self.service).await.map(|_| ())
    }

    /// Keeps `name`, one of the secrets the service declares, which its owner holds already (something else made it, and
    /// the vault took it in): marked as its owner's to manage from now on and given `rule` (its value as it is), once
    /// however many render at once, so that `vault_rotate level:service` rotates it under that rule. Nothing is made:
    /// refused where there is none.
    pub async fn keep(&self, name: &str, rule: Rule) -> Result<()> {
        if !self.made.contains(&name) {
            return Err(Error::Invalid(format!("the service {} does not declare that the vault keeps {name} for it", self.service)));
        }
        if self.rotated.is_some() {
            return self.renew(name, rule).await;
        }
        self.vault.keep_for(self.kernel, name, rule, self.service).await.map(|_| ())
    }

    /// In a rotation's render: `name` made anew, once, under `rule` as the service declares it now.
    async fn renew(&self, name: &str, rule: Rule) -> Result<()> {
        let rotated = self.rotated.as_ref().ok_or_else(|| Error::Internal("not a rotation".into()))?;
        if rotated.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| d.name == name) {
            return Ok(());
        }
        let d = self.vault.rotate_made(self.kernel, name, self.service, Some(rule)).await?;
        rotated.lock().unwrap_or_else(|e| e.into_inner()).push(d);
        Ok(())
    }

    /// The current value of its owner's secret `name` of the installation, for a file: only where the secret's rule lets
    /// its owner read it. On the record as `render`, naming the service; never the value.
    pub async fn secret(&self, name: &str) -> Result<Zeroizing<Vec<u8>>> {
        Ok(self.given(name, false).await?.into_iter().next().map_or_else(|| Zeroizing::new(Vec::new()), |v| v.value))
    }

    /// [`Rendering::secret`], and after it the version before while the rotation's overlap lasts: for a program that
    /// holds two values at once, so that neither it nor the server refuses the other while a change goes round.
    pub async fn versions(&self, name: &str) -> Result<Vec<Version>> {
        self.given(name, true).await
    }

    async fn given(&self, name: &str, previous: bool) -> Result<Vec<Version>> {
        if self.rotated.is_some() {
            return Ok(Vec::new());
        }
        let versions = match self.vault.rendered(self.kernel, name, self.service, previous).await {
            Err(Error::NotFound(_)) => {
                return Err(Error::Invalid(format!(
                    "the service {} needs the secret {name} of {}, which the vault does not hold",
                    self.service,
                    self.vault.owner()
                )));
            }
            other => other?,
        };
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.extend(versions.iter().map(|(_, v)| v.clone()));
        Ok(versions.into_iter().map(|(version, value)| Version { version, value }).collect())
    }
}

/// A service as `ferrum task services` lists it: never what its files hold.
#[derive(Debug, Clone, Serialize)]
pub struct Declared {
    pub name: String,
    pub owner: String,
    pub files: Vec<String>,
    pub made: Vec<String>,
    /// Why it cannot be rendered here ([`Companion::refusal`]; the vault closed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// A render's files, to be written by [`write`].
pub struct Rendered {
    pub service: String,
    pub owner: String,
    pub group_reads: bool,
    pub files: Vec<File>,
}

impl std::fmt::Debug for Rendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendered").field("service", &self.service).field("owner", &self.owner).field("files", &self.files).finish()
    }
}

/// Whether `name` can be a service's: lowercase letters, digits and dashes, as an app's id.
pub(crate) fn service_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=63).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Whether `name` can be a file's: letters, digits, `.`, `_` and `-`, up to 100, not beginning with a dot (the writer's
/// own) and naming no other directory.
pub(crate) fn file_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=100).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// What is wrong with what a service declares, said when the server starts; nothing when it is sound.
pub(crate) fn declaration_problems(name: &str, c: &dyn Companion) -> Vec<String> {
    let mut out = Vec::new();
    if !service_name_ok(name) {
        out.push(format!("{name:?} is not a service's name (lowercase letters, digits and dashes)"));
    }
    let files = c.files();
    if files.is_empty() {
        out.push(format!("the service {name} declares no file"));
    }
    for (i, f) in files.iter().enumerate() {
        if !file_name_ok(f) {
            out.push(format!("the service {name} declares the file {f:?}: letters, digits, . _ and -, not beginning with a dot"));
        } else if files[..i].contains(f) {
            out.push(format!("the service {name} declares the file {f} twice"));
        }
    }
    for m in c.made() {
        if !crate::vault::secret_name_ok(m) {
            out.push(format!("the service {name} declares the secret {m:?}, which is not a secret's name the vault takes"));
        }
    }
    out
}

impl Kernel {
    /// Every service declared on this server, with its owner, its files, the secrets the vault keeps for it, and why it
    /// cannot be rendered here, if it cannot (the settings as `settings` resolves them).
    pub async fn companions(&self, settings: &(dyn Fn(&str) -> Option<String> + Sync)) -> Vec<Declared> {
        let mut out = Vec::new();
        for (name, owner, c) in self.hooks.companions() {
            let refused = match self.rendering(name, owner, c, settings) {
                Ok(r) => c.refusal(&r).await.unwrap_or_else(|e| Some(e.to_string())),
                Err(e) => Some(e.to_string()),
            };
            out.push(Declared {
                name: name.to_string(),
                owner: owner.to_string(),
                files: c.files().iter().map(|f| f.to_string()).collect(),
                made: c.made().iter().map(|m| m.to_string()).collect(),
                refused,
            });
        }
        out
    }

    /// What the service `name` is given as it renders: refused while the vault is closed.
    fn rendering<'a>(
        &'a self,
        name: &'a str,
        owner: &str,
        companion: &dyn Companion,
        settings: &'a (dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Rendering<'a>> {
        self.db.vault.installation()?;
        let vault = crate::vault::handle_of(self, owner)?;
        Ok(Rendering { kernel: self, vault, service: name, made: companion.made(), settings, given: Mutex::default(), rotated: None })
    }

    /// Refused, saying why, where the service says this installation may not have it rendered.
    async fn may_render(&self, name: &str, companion: &dyn Companion, r: &Rendering<'_>) -> Result<()> {
        match companion.refusal(r).await? {
            Some(why) => Err(Error::Conflict(format!("the service {name} is not rendered here: {why}"))),
            None => Ok(()),
        }
    }

    /// The files of the service `name`, from the settings as `settings` resolves them and its owner's secrets: refused
    /// while the vault is closed (whatever the service needs), where the service says this installation may not have it
    /// ([`Companion::refusal`], asked first), and when the function fails, returns a file it did not declare (or not one
    /// it did), or puts a secret's value in a file that says it holds none.
    pub async fn render(&self, name: &str, settings: &(dyn Fn(&str) -> Option<String> + Sync)) -> Result<Rendered> {
        let (owner, companion) = self.companion(name)?;
        let r = self.rendering(name, owner, companion, settings)?;
        self.may_render(name, companion, &r).await?;
        let files = companion.render(&r).await?;
        let given = r.given.into_inner().unwrap_or_else(|e| e.into_inner());
        checked(name, companion.files(), &files, &given)?;
        Ok(Rendered { service: name.to_string(), owner: owner.to_string(), group_reads: companion.group_reads(), files })
    }

    /// New values of the secrets the vault keeps for the service `name` ([`Companion::made`]), for the operator (`ferrum
    /// task vault_rotate level:service`): each of the same kind, under the rule the service declares now (its render is
    /// run, told they are not there yet, and its files are not written: the hosts it binds a secret to come from the
    /// settings as they are, so an address that moved takes the new value and no other), so that the version before still
    /// verifies, under its own rule, and is given to the service's renders for the overlap. One whose rule the service
    /// does not declare keeps its rule. A render alone never changes a secret's rule. Only secrets the vault keeps for a
    /// service (their app manages them), and refused, changing nothing, where the service may not be rendered here.
    pub async fn rotate_companion(&self, name: &str, settings: &(dyn Fn(&str) -> Option<String> + Sync)) -> Result<Vec<Described>> {
        let (owner, companion) = self.companion(name)?;
        let mut r = self.rendering(name, owner, companion, settings)?;
        self.may_render(name, companion, &r).await?;
        if companion.made().is_empty() {
            return Err(Error::Invalid(format!("the vault keeps none of the service {name}'s secrets: nothing to rotate here")));
        }
        r.vault = r.vault.renewing(companion.made());
        r.rotated = Some(Mutex::default());
        companion.render(&r).await?;
        let mut out = r.rotated.take().map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner())).unwrap_or_default();
        for secret in companion.made() {
            if !out.iter().any(|d| d.name == *secret) {
                out.push(r.vault.rotate_made(self, secret, name, None).await?);
            }
        }
        Ok(out)
    }

    fn companion(&self, name: &str) -> Result<(&str, &dyn Companion)> {
        self.hooks.companion(name).ok_or_else(|| Error::NotFound(format!("service {name} on this server (ferrum task services lists them)")))
    }
}

/// The values given to a render that a file saying it holds no secret is checked for: shorter ones would be found by
/// chance in ordinary text.
const CHECKED_FROM: usize = 16;

/// Exactly the files `declared`, each once, and no secret's value in one that says it holds none.
fn checked(service: &str, declared: &[&str], files: &[File], given: &[Zeroizing<Vec<u8>>]) -> Result<()> {
    let mut names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    let mut wanted: Vec<&str> = declared.to_vec();
    wanted.sort_unstable();
    if names != wanted {
        return Err(Error::Internal(format!("the service {service} rendered {} where it declares {}", names.join(", "), wanted.join(", "))));
    }
    for f in files.iter().filter(|f| !f.secret) {
        if given.iter().filter(|v| v.len() >= CHECKED_FROM).any(|v| f.bytes.windows(v.len()).any(|w| w == v.as_slice())) {
            return Err(Error::Internal(format!("the service {service} put a secret in {}, which it says holds none: nothing is written", f.name)));
        }
    }
    Ok(())
}

/// The file beside a service's files that says which service wrote them, and which.
pub const RECORD: &str = ".ferrum-render";
/// How the files a render is writing begin, until each is renamed to its name.
const TEMPORARY: &str = ".ferrum-render-tmp.";

/// What [`write`] did.
#[derive(Debug, Clone, Default)]
pub struct Written {
    /// Each file written, and whether it holds a secret.
    pub files: Vec<(String, bool)>,
    /// What an earlier render of the service wrote and this one does not: gone.
    pub removed: Vec<String>,
}

/// Writes `rendered` into `dir`: each file under a temporary name first, then renamed to its own, readable by its owner
/// alone (or its group too, where the service says so). `dir` is made when it is missing; it must be a directory, not a
/// link, empty or holding only what an earlier render of the same service wrote (and the temporary files of one that
/// stopped half way). Nothing is written through a link, nor outside `dir`; what the earlier render wrote and this one
/// does not is removed. Renders into one directory take turns (a lock on it). Refused, with nothing written, when the
/// directory is not one of these.
pub fn write(dir: &Path, rendered: &Rendered) -> Result<Written> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let (file_mode, dir_mode) = if rendered.group_reads { (0o640, 0o750) } else { (0o600, 0o700) };
    let shown = dir.display();
    let io = |what: &str, e: std::io::Error| Error::Unavailable(format!("{what}: {e}"));
    for f in &rendered.files {
        if !file_name_ok(&f.name) {
            return Err(Error::Invalid(format!("{:?} is not a file's name a render writes", f.name)));
        }
    }
    // Made unless something is there (another render may make it at the same moment); whatever is there is then looked at
    // as it is: mkdir never follows a link.
    if let Err(e) = std::fs::DirBuilder::new().mode(dir_mode).create(dir)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(io(&format!("making {shown}"), e));
    }
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => return Err(Error::Invalid(format!("{shown} is a link: give the directory itself"))),
        Ok(m) if !m.is_dir() => return Err(Error::Invalid(format!("{shown} is not a directory"))),
        Ok(_) => {}
        Err(e) => return Err(io(&shown.to_string(), e)),
    }
    // Its contents are looked at only once this render holds the lock: before it, another render may be half way through
    // its renames (its files there, its record not yet), which is no directory to refuse. Nothing is written before.
    let handle = std::fs::File::open(dir).map_err(|e| io(&shown.to_string(), e))?;
    handle.lock().map_err(|e| io(&format!("taking {shown} for this render"), e))?;
    let (before, leftovers) = earlier(dir, &rendered.service)?;

    let names: Vec<&str> = rendered.files.iter().map(|f| f.name.as_str()).collect();
    let record = serde_json::to_vec(&serde_json::json!({"service": rendered.service, "files": names})).map_err(Error::internal)?;
    let mut staged: Vec<(std::path::PathBuf, &str)> = Vec::new();
    let staging = (|| -> Result<()> {
        for (bytes, name) in rendered.files.iter().map(|f| (f.bytes.as_slice(), f.name.as_str())).chain([(record.as_slice(), RECORD)]) {
            let path = dir.join(format!("{TEMPORARY}{:016x}", rand::random::<u64>()));
            // A new file, never one that is there (a link included: O_EXCL does not follow it).
            let mut file =
                std::fs::OpenOptions::new().write(true).create_new(true).mode(file_mode).open(&path).map_err(|e| io(&format!("writing in {shown}"), e))?;
            staged.push((path, name));
            // The mode exactly, whatever the process's umask took away.
            file.set_permissions(std::fs::Permissions::from_mode(file_mode)).map_err(|e| io(&format!("writing in {shown}"), e))?;
            std::io::Write::write_all(&mut file, bytes).map_err(|e| io(&format!("writing {name} in {shown}"), e))?;
            file.sync_all().map_err(|e| io(&format!("writing {name} in {shown}"), e))?;
        }
        Ok(())
    })();
    if let Err(e) = staging {
        for (path, _) in &staged {
            let _ = std::fs::remove_file(path);
        }
        return Err(e);
    }
    // ponytail: a crash between these renames leaves some files of each render; the program is not started on it (the
    // render failed), and the next render replaces them all. Atomic as a whole would take a link swapped in their place.
    for (path, name) in &staged {
        std::fs::rename(path, dir.join(name)).map_err(|e| io(&format!("writing {name} in {shown}"), e))?;
    }
    let mut removed = Vec::new();
    for old in before.iter().filter(|o| !names.contains(&o.as_str())) {
        match std::fs::remove_file(dir.join(old)) {
            Ok(()) => removed.push(old.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&format!("removing {old} from {shown}"), e)),
        }
    }
    for left in leftovers {
        let _ = std::fs::remove_file(dir.join(left));
    }
    // The renames themselves kept, where the file system lets a directory be synced (macOS's full sync may not).
    let _ = handle.sync_all();
    Ok(Written { files: rendered.files.iter().map(|f| (f.name.clone(), f.secret)).collect(), removed })
}

/// What an earlier render of `service` wrote in `dir` (the files its record names), and the temporary files of one that
/// stopped half way; refused when `dir` holds anything else, another service's files, or a link or a directory where a
/// file of the render's is.
fn earlier(dir: &Path, service: &str) -> Result<(Vec<String>, Vec<String>)> {
    let shown = dir.display();
    let io = |e: std::io::Error| Error::Unavailable(format!("{shown}: {e}"));
    let not_plain = |name: &str| Error::Invalid(format!("{shown}/{name} is not a plain file (a link, or a directory): nothing is written through it"));
    let (mut record, mut others, mut leftovers) = (None, Vec::new(), Vec::new());
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name().into_string().map_err(|n| Error::Invalid(format!("{shown} holds {n:?}, which no render wrote")))?;
        // The entry itself, never what a link points at.
        let plain = entry.file_type().map_err(io)?.is_file();
        if name == RECORD {
            if !plain {
                return Err(not_plain(&name));
            }
            record = Some(std::fs::read(dir.join(RECORD)).map_err(io)?);
        } else if name.starts_with(TEMPORARY) {
            if !plain {
                return Err(not_plain(&name));
            }
            leftovers.push(name);
        } else {
            others.push((name, plain));
        }
    }
    let listed: Vec<String> = match record {
        None => Vec::new(),
        Some(bytes) => {
            let said: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            match said["service"].as_str() {
                Some(s) if s == service => {}
                Some(other) => return Err(Error::Invalid(format!("{shown} holds the files of the service {other}: give {service} a directory of its own"))),
                None => return Err(Error::Invalid(format!("{shown}/{RECORD} does not say which service wrote it: give an empty directory"))),
            }
            said["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).filter(|f| file_name_ok(f)).map(str::to_string).collect()
        }
    };
    for (name, plain) in others {
        if !listed.contains(&name) {
            return Err(Error::Invalid(format!(
                "{shown} holds {name}, which no render of {service} wrote: give a directory that is empty or holds only its files"
            )));
        }
        if !plain {
            return Err(not_plain(&name));
        }
    }
    Ok((listed, leftovers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn rendered(service: &str, files: &[(&str, &str)], group_reads: bool) -> Rendered {
        Rendered {
            service: service.into(),
            owner: "platform".into(),
            group_reads,
            files: files.iter().map(|(n, b)| File::new(n, b.as_bytes().to_vec(), true)).collect(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrum-companion-{name}-{}-{:x}", std::process::id(), rand::random::<u32>()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn names_are_plain() {
        for ok in ["meet", "object-store", "a1"] {
            assert!(service_name_ok(ok), "{ok}");
        }
        for not in ["", "Meet", "-x", "a_b", "a/b", &"a".repeat(64)] {
            assert!(!service_name_ok(not), "{not}");
        }
        for ok in ["livekit.yaml", "s3.json", "token", "keys_1-2.txt"] {
            assert!(file_name_ok(ok), "{ok}");
        }
        for not in ["", ".ferrum-render", "..", ".", "a/b", "../x", "a b", "x\n", &"a".repeat(101)] {
            assert!(!file_name_ok(not), "{not:?}");
        }
    }

    #[test]
    fn a_render_writes_its_files_for_their_owner_alone_and_takes_back_what_it_no_longer_writes() {
        let dir = scratch("write");
        let w = write(&dir, &rendered("meet", &[("livekit.yaml", "keys: {}\n"), ("old.txt", "x")], false)).unwrap();
        assert_eq!(w.files.len(), 2);
        assert_eq!(listing(&dir), [".ferrum-render", "livekit.yaml", "old.txt"], "no temporary file is left");
        assert_eq!((mode(&dir), mode(&dir.join("livekit.yaml")), mode(&dir.join(RECORD))), (0o700, 0o600, 0o600));
        // Again, with one file fewer and a new value: written over, and the one no longer written goes.
        let w = write(&dir, &rendered("meet", &[("livekit.yaml", "keys: {a: b}\n")], false)).unwrap();
        assert_eq!(w.removed, ["old.txt"]);
        assert_eq!(listing(&dir), [".ferrum-render", "livekit.yaml"]);
        assert_eq!(std::fs::read_to_string(dir.join("livekit.yaml")).unwrap(), "keys: {a: b}\n");
        // A group may read them where the service says so.
        let group = scratch("group");
        write(&group, &rendered("meet", &[("livekit.yaml", "x")], true)).unwrap();
        assert_eq!((mode(&group), mode(&group.join("livekit.yaml"))), (0o750, 0o640));
        // What a render stopped half way left is cleared by the next one.
        std::fs::write(dir.join(format!("{TEMPORARY}0000")), "half").unwrap();
        write(&dir, &rendered("meet", &[("livekit.yaml", "y")], false)).unwrap();
        assert_eq!(listing(&dir), [".ferrum-render", "livekit.yaml"]);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&group).unwrap();
    }

    #[test]
    fn renders_into_one_directory_at_once_take_turns_and_each_is_whole() {
        let dir = scratch("together");
        // Large enough that each is still writing while another looks at the directory.
        let bodies: Vec<String> = (0..8).map(|i| format!("keys: {{ferrum.{i}: {}}}\n", "x".repeat((256 << 10) + i))).collect();
        std::thread::scope(|s| {
            for body in &bodies {
                let dir = &dir;
                s.spawn(move || write(dir, &rendered("meet", &[("livekit.yaml", body)], false)).expect("each render goes through"));
            }
        });
        assert_eq!(listing(&dir), [".ferrum-render", "livekit.yaml"], "nothing half way left behind");
        assert!(bodies.contains(&std::fs::read_to_string(dir.join("livekit.yaml")).unwrap()), "one render's file, whole");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_that_is_not_the_services_own_is_refused_and_left_as_it_was() {
        // Something else in it.
        let dir = scratch("foreign");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let e = write(&dir, &rendered("meet", &[("livekit.yaml", "x")], false)).unwrap_err().to_string();
        assert!(e.contains("holds notes.txt, which no render of meet wrote"), "{e}");
        assert_eq!(listing(&dir), ["notes.txt"], "nothing written");
        // Another service's files.
        let other = scratch("other");
        write(&other, &rendered("mail", &[("config.toml", "x")], false)).unwrap();
        let e = write(&other, &rendered("meet", &[("livekit.yaml", "x")], false)).unwrap_err().to_string();
        assert!(e.contains("holds the files of the service mail"), "{e}");
        assert_eq!(listing(&other), [".ferrum-render", "config.toml"]);
        // A link where one of its files goes: never written through, and what it points at is left alone.
        let linked = scratch("linked");
        write(&linked, &rendered("meet", &[("livekit.yaml", "x")], false)).unwrap();
        let target = scratch("target");
        std::fs::write(&target, "elsewhere").unwrap();
        std::fs::remove_file(linked.join("livekit.yaml")).unwrap();
        std::os::unix::fs::symlink(&target, linked.join("livekit.yaml")).unwrap();
        let e = write(&linked, &rendered("meet", &[("livekit.yaml", "secret")], false)).unwrap_err().to_string();
        assert!(e.contains("not a plain file"), "{e}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "elsewhere", "nothing written through the link");
        // The directory itself a link.
        let real = scratch("real");
        std::fs::create_dir(&real).unwrap();
        let alias = scratch("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let e = write(&alias, &rendered("meet", &[("livekit.yaml", "x")], false)).unwrap_err().to_string();
        assert!(e.contains("is a link"), "{e}");
        assert!(listing(&real).is_empty());
        // A name that would leave the directory.
        let e = write(&real, &rendered("meet", &[("../escape", "x")], false)).unwrap_err().to_string();
        assert!(e.contains("not a file's name"), "{e}");
        assert!(listing(&real).is_empty() && !real.parent().unwrap().join("escape").exists());
        for d in [&dir, &other, &linked, &real] {
            std::fs::remove_dir_all(d).unwrap();
        }
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_file(&alias).unwrap();
    }

    #[test]
    fn what_a_service_returns_is_what_it_declares_and_no_secret_hides_in_a_plain_file() {
        let given = [Zeroizing::new(b"0123456789abcdef-secret".to_vec())];
        let file = |name: &str, body: &str, secret: bool| File::new(name, body.as_bytes().to_vec(), secret);
        assert!(checked("s", &["a", "b"], &[file("b", "x", false), file("a", "y", true)], &given).is_ok(), "in any order");
        let e = checked("s", &["a", "b"], &[file("a", "y", true)], &given).unwrap_err().to_string();
        assert!(e.contains("rendered a where it declares a, b"), "{e}");
        assert!(checked("s", &["a"], &[file("a", "y", true), file("a", "y", true)], &given).is_err(), "twice");
        assert!(checked("s", &["a"], &[file("c", "y", true)], &given).is_err(), "undeclared");
        let e = checked("s", &["a"], &[file("a", "key: 0123456789abcdef-secret\n", false)], &given).unwrap_err().to_string();
        assert!(e.contains("put a secret in a, which it says holds none"), "{e}");
        assert!(checked("s", &["a"], &[file("a", "key: 0123456789abcdef-secret\n", true)], &given).is_ok(), "where it says so");
        assert_eq!(format!("{:?}", file("a", "hunter2-hunter2-hunter2", true)), "File(a, 23 bytes, a secret)", "never the bytes");
    }
}
