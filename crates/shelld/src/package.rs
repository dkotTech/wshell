//! Package manager: unpacking, checking, AOT compilation and installation.
//!
//! A package is a directory or a tar archive (`.pkg`) with `app.toml`, a WASM component and `ui/`.
//! Package signing comes after the MVP.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use shell_core::config::Config;
use shell_core::control::{PackageInfo, PermissionInfo};
use shell_core::exec_template::Template;
use shell_core::manifest::{BackendKind, ExecSpec, Manifest, Permission, PermissionKind};
use sha2::{Digest, Sha256};
use wasmtime::Engine;

/// AOT image fingerprint next to `backend.cwasm`: `<file sha256> <engine fingerprint>`.
const CWASM_STAMP: &str = "backend.cwasm.stamp";

/// Fingerprint of engine settings and version: if it matches, images are guaranteed compatible.
fn engine_fingerprint(engine: &Engine) -> String {
    use std::hash::{Hash, Hasher};
    struct ShaHasher(Sha256);
    impl Hasher for ShaHasher {
        fn write(&mut self, bytes: &[u8]) {
            self.0.update(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    let mut h = ShaHasher(Sha256::new());
    engine.precompile_compatibility_hash().hash(&mut h);
    crate::util::hex(&h.0.finalize()[..16])
}

/// Records the fingerprint of the package's AOT image; returns the image's SHA-256.
fn write_stamp(pkg_dir: &Path, engine: &Engine) -> Result<String> {
    let sha = sha256_file(&pkg_dir.join("backend.cwasm"))?;
    crate::util::write_atomic(&pkg_dir.join(CWASM_STAMP), format!("{sha} {}", engine_fingerprint(engine)).as_bytes())?;
    Ok(sha)
}

/// Checks the AOT image before starting: whether the file is intact and matches the engine (after
/// a shelld update). Otherwise recompiles it from `.wasm`, once, outside the start
/// timeout. Returns the SHA-256 of a valid image and whether it was recompiled. Blocking.
pub fn ensure_cwasm(installed: &Installed, engine: &Engine, threads: usize) -> Result<(String, bool)> {
    let stamp = fs::read_to_string(installed.pkg_dir.join(CWASM_STAMP)).unwrap_or_default();
    let mut parts = stamp.split_whitespace();
    if let (Some(sha), Some(fingerprint)) = (parts.next(), parts.next())
        && fingerprint == engine_fingerprint(engine)
        && sha256_file(&installed.cwasm_path()).is_ok_and(|h| h == sha)
    {
        return Ok((sha.to_string(), false));
    }
    crate::engine::compile(&installed.wasm_path(), &installed.cwasm_path(), threads).context("recompiling component")?;
    Ok((write_stamp(&installed.pkg_dir, engine)?, true))
}

/// SHA-256 of a file (hex), streamed, without reading it entirely into memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(crate::util::hex(&hasher.finalize()))
}

/// Data directory layout.
#[derive(Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn app(&self, id: &str) -> PathBuf {
        self.root.join("apps").join(id)
    }
    pub fn pkg(&self, id: &str) -> PathBuf {
        self.app(id).join("pkg")
    }
    pub fn grants(&self, id: &str) -> PathBuf {
        self.app(id).join("grants.toml")
    }
    pub fn app_data(&self, id: &str) -> PathBuf {
        self.root.join("data").join(id)
    }
    pub fn log(&self, id: &str) -> PathBuf {
        self.root.join("logs").join(format!("{id}.log"))
    }
    pub fn audit(&self) -> PathBuf {
        self.root.join("audit.log")
    }
    pub fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn create(&self) -> Result<()> {
        for d in ["apps", "data", "logs", "staging"] {
            fs::create_dir_all(self.root.join(d))?;
        }
        set_private(&self.root)?;
        Ok(())
    }
}

fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", path.display()))
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GrantsFile {
    pub granted: BTreeSet<String>,
}

/// An installed app.
#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    pub pkg_dir: PathBuf,
    pub granted: BTreeSet<String>,
    /// Granted permissions with parameters, parsed once at load time.
    pub grants: Grants,
}

/// Parameters of granted permissions: what host services check on every call.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    pub storage_quota_mb: Option<u32>,
    pub http: Option<HttpGrant>,
    /// `exec` permission name → spec and parsed argument template.
    pub exec: BTreeMap<String, ExecGrant>,
    /// `net.listen` ports: the program may bind/listen on them.
    pub listen: Vec<u16>,
    /// `shell.apps` (plugins): the installed apps and links to their UI.
    pub apps: bool,
}

#[derive(Debug, Clone)]
pub struct HttpGrant {
    /// Lowercase; `*.example.com` means subdomains.
    pub hosts: Vec<String>,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExecGrant {
    pub spec: ExecSpec,
    pub template: Template,
}

impl Grants {
    fn resolve(manifest: &Manifest, granted: &BTreeSet<String>) -> Result<Self> {
        let mut g = Grants::default();
        for p in manifest.permissions.iter().filter(|p| granted.contains(&p.key())) {
            match &p.kind {
                PermissionKind::StoragePrivate { quota_mb } => g.storage_quota_mb = Some(*quota_mb),
                PermissionKind::NetHttp { hosts, methods } => {
                    let hosts = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
                    g.http = Some(HttpGrant { hosts, methods: methods.clone() });
                }
                PermissionKind::Exec(spec) => {
                    let grant = ExecGrant { spec: spec.clone(), template: spec.template()? };
                    g.exec.insert(spec.name().to_string(), grant);
                }
                PermissionKind::NetListen { ports } => g.listen.extend(ports),
                PermissionKind::ShellApps {} => g.apps = true,
                PermissionKind::SystemServices { .. } | PermissionKind::SystemNetwork { .. } => {}
            }
        }
        Ok(g)
    }
}

impl Installed {
    pub fn load(paths: &Paths, id: &str) -> Result<Self> {
        let pkg_dir = paths.pkg(id);
        let manifest = Manifest::parse(&fs::read_to_string(pkg_dir.join("app.toml"))?)?;
        ensure!(manifest.app.id == id, "id in the manifest does not match the directory");
        let file: GrantsFile = toml::from_str(&fs::read_to_string(paths.grants(id))?)?;
        let grants = Grants::resolve(&manifest, &file.granted)?;
        Ok(Installed { manifest, pkg_dir, granted: file.granted, grants })
    }

    pub fn is_granted(&self, key: &str) -> bool {
        self.granted.contains(key)
    }

    /// Manifest permissions for display, marked as granted or not.
    pub fn permission_infos(&self) -> Vec<PermissionInfo> {
        self.manifest.permissions.iter().map(|p| permission_info(p, self.is_granted(&p.key()))).collect()
    }

    pub fn wasm_path(&self) -> PathBuf {
        self.pkg_dir.join(&self.manifest.backend.component)
    }

    pub fn cwasm_path(&self) -> PathBuf {
        self.pkg_dir.join("backend.cwasm")
    }
}

pub fn load_all(paths: &Paths) -> Vec<(String, Result<Installed>)> {
    let Ok(rd) = fs::read_dir(paths.root.join("apps")) else { return Vec::new() };
    let mut out: Vec<_> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| shell_core::manifest::validate_app_id(id).is_ok())
        .map(|id| {
            let r = Installed::load(paths, &id);
            (id, r)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A package that has been unpacked and checked but not yet installed.
pub struct Staged {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// The component's AOT image in the staging cache.
    pub cwasm: PathBuf,
    pub warnings: Vec<String>,
    _tmp: TmpDir,
}

/// Temporary unpack directory, removed on drop.
struct TmpDir(Option<PathBuf>);

impl Drop for TmpDir {
    fn drop(&mut self) {
        if let Some(tmp) = &self.0 {
            let _ = fs::remove_dir_all(tmp);
        }
    }
}

/// Unpacks (if it is an archive) and fully checks a package. Blocking.
pub fn stage(path: &Path, paths: &Paths, cfg: &Config, engine: &Engine) -> Result<Staged> {
    let meta = fs::metadata(path).with_context(|| format!("package {}", path.display()))?;
    let (dir, tmp) = if meta.is_dir() {
        (path.to_path_buf(), TmpDir(None))
    } else {
        let tmp = paths.staging().join(crate::util::token());
        fs::create_dir_all(&tmp)?;
        let guard = TmpDir(Some(tmp.clone()));
        unpack(path, &tmp).context("unpacking package (expected a tar archive)")?;
        (tmp, guard)
    };
    let (manifest, cwasm, warnings) = verify(&dir, paths, cfg, engine)?;
    Ok(Staged { dir, manifest, cwasm, warnings, _tmp: tmp })
}

fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut ar = tar::Archive::new(file);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let path = entry.path()?.into_owned();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "entry {} ({kind:?}) is not allowed in a package",
            path.display()
        );
        ensure!(entry.unpack_in(dest)?, "invalid path in package: {}", path.display());
    }
    Ok(())
}

fn verify(dir: &Path, paths: &Paths, cfg: &Config, engine: &Engine) -> Result<(Manifest, PathBuf, Vec<String>)> {
    let mut warnings = Vec::new();
    let manifest_path = dir.join("app.toml");
    let text = fs::read_to_string(&manifest_path).context("package has no app.toml")?;
    let manifest = Manifest::parse(&text)?;
    ensure!(
        manifest.backend.kind != BackendKind::Plugin || cfg.features.plugins,
        "plugins are disabled by the device configuration ([features] plugins = false)"
    );

    for p in &manifest.permissions {
        if let PermissionKind::Exec(e) = &p.kind {
            for privilege in &e.privileges {
                ensure!(
                    cfg.exec.allowed_privileges.contains(privilege),
                    "permission {}: privilege {privilege} is forbidden by the device policy",
                    p.key()
                );
            }
            if !Path::new(&e.binary).is_file() {
                let msg = format!("permission {}: {} not found on the device", p.key(), e.binary);
                ensure!(p.optional, "{msg}");
                warnings.push(msg);
            }
        }
    }

    if let Some(ui) = &manifest.ui {
        let entry = dir.join(&ui.entry);
        ensure!(entry.is_file(), "missing UI entry point {}", ui.entry);
    }

    let wasm = dir.join(&manifest.backend.component);
    ensure!(no_symlink(&wasm)?, "{} is not a regular file", manifest.backend.component);
    let cwasm = compile_cached(&wasm, paths, cfg, engine)
        .with_context(|| format!("compiling {}", manifest.backend.component))?;
    let component = crate::engine::load_cwasm(engine, &cwasm)?;
    crate::engine::check_component(engine, &component, &manifest)?;
    crate::worker::typecheck(engine, &component, manifest.backend.kind)
        .with_context(|| format!("component does not match world {}", manifest.backend.world))?;

    Ok((manifest, cwasm, warnings))
}

/// Compiles a component once per content: the permission prompt (`inspect`) and the
/// following `install` use the same image. Only the latest image is kept. Blocking.
fn compile_cached(wasm: &Path, paths: &Paths, cfg: &Config, engine: &Engine) -> Result<PathBuf> {
    let name = format!("{}-{}.cwasm", sha256_file(wasm)?, engine_fingerprint(engine));
    let dir = paths.staging();
    let cwasm = dir.join(&name);
    if cwasm.is_file() {
        return Ok(cwasm);
    }
    for entry in fs::read_dir(&dir)?.flatten() {
        if entry.path().extension().is_some_and(|e| e == "cwasm") {
            let _ = fs::remove_file(entry.path());
        }
    }
    crate::engine::compile(wasm, &cwasm, cfg.shell.compile_threads)?;
    Ok(cwasm)
}

fn no_symlink(p: &Path) -> Result<bool> {
    Ok(fs::symlink_metadata(p).with_context(|| format!("{}", p.display()))?.is_file())
}

pub fn package_info(staged: &Staged, current: Option<&Installed>) -> PackageInfo {
    let m = &staged.manifest;
    PackageInfo {
        id: m.app.id.clone(),
        name: m.app.name.clone(),
        version: m.app.version.clone(),
        has_ui: m.ui.is_some(),
        installed_version: current.map(|c| c.manifest.app.version.clone()),
        permissions: m
            .permissions
            .iter()
            .map(|p| permission_info(p, current.is_some_and(|c| c.is_granted(&p.key()))))
            .collect(),
        warnings: staged.warnings.clone(),
    }
}

fn permission_info(p: &Permission, granted: bool) -> PermissionInfo {
    PermissionInfo {
        key: p.key(),
        description: p.kind.to_string(),
        reason: p.reason.clone(),
        optional: p.optional,
        granted,
    }
}

/// Copies the package into the data directory, saves the AOT image and granted permissions.
pub fn install(
    staged: &Staged,
    grant_optional: &[String],
    paths: &Paths,
    engine: &Engine,
    audit: &crate::logs::Audit,
) -> Result<Installed> {
    let m = &staged.manifest;
    let id = &m.app.id;

    let mut granted = BTreeSet::new();
    for key in grant_optional {
        let p = m.permission(key).with_context(|| format!("permission {key} is not declared in the manifest"))?;
        ensure!(p.optional, "permission {key} is required; it is granted together with installation");
    }
    for p in &m.permissions {
        let key = p.key();
        if !p.optional || grant_optional.contains(&key) {
            granted.insert(key);
        }
    }

    let app_dir = paths.app(id);
    fs::create_dir_all(&app_dir)?;
    let new_pkg = app_dir.join("pkg.new");
    let _ = fs::remove_dir_all(&new_pkg);
    fs::create_dir_all(&new_pkg)?;

    let result = (|| -> Result<()> {
        fs::copy(staged.dir.join("app.toml"), new_pkg.join("app.toml"))?;
        let comp = &m.backend.component;
        if let Some(parent) = Path::new(comp).parent() {
            fs::create_dir_all(new_pkg.join(parent))?;
        }
        fs::copy(staged.dir.join(comp), new_pkg.join(comp))?;
        if m.ui.is_some() {
            copy_tree(&staged.dir.join("ui"), &new_pkg.join("ui"))?;
        }
        fs::copy(&staged.cwasm, new_pkg.join("backend.cwasm"))?;
        write_stamp(&new_pkg, engine)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&new_pkg);
        return Err(e.context("copying package"));
    }

    let pkg = paths.pkg(id);
    let old = app_dir.join("pkg.old");
    let _ = fs::remove_dir_all(&old);
    if pkg.exists() {
        fs::rename(&pkg, &old)?;
    }
    fs::rename(&new_pkg, &pkg)?;
    let _ = fs::remove_dir_all(&old);
    let _ = fs::remove_file(&staged.cwasm);

    let grants = GrantsFile { granted: granted.clone() };
    crate::util::write_atomic(&paths.grants(id), toml::to_string(&grants)?.as_bytes())?;

    for p in &m.permissions {
        let key = p.key();
        let event = if granted.contains(&key) { "grant" } else { "deny" };
        audit.record(id, event, &key);
    }
    audit.record(id, "install", &m.app.version);

    Installed::load(paths, id)
}

pub fn uninstall(paths: &Paths, id: &str) -> Result<()> {
    let dir = paths.app(id);
    if !dir.exists() {
        bail!("app {id} is not installed");
    }
    fs::remove_dir_all(&dir)?;
    let _ = fs::remove_dir_all(paths.app_data(id));
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &to)?;
        } else {
            bail!("entry {} is not allowed in ui/ (symlink or special file)", entry.path().display());
        }
    }
    Ok(())
}
