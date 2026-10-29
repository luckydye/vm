use crate::{
    instance::{self, Instance},
    machine,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

/// No device needs bounce buffers, so swiotlb would only pin 64 MiB of guest RAM.
/// Free page reporting releases free blocks down to one 16 KiB host page (order 2)
/// instead of 2 MiB, so fragmented free memory goes back to the host too.
const KERNEL_ARGS: &str = "root=LABEL=varmint-root rw quiet loglevel=3 apparmor=0 \
     systemd.condition-needs-update=0 swiotlb=noforce page_reporting.page_reporting_order=2";
const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// Virtual hardware of one VM, stored as `meta.json` in its directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    pub persist: bool,
    pub cpus: usize,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub width: u32,
    pub height: u32,
}

impl Hardware {
    pub fn validate(&self) -> Result<(), String> {
        if self.cpus == 0 {
            return Err("cpus must be greater than zero".into());
        }
        if self.memory_mib < 256 {
            return Err("memory must be at least 256 MiB".into());
        }
        if self.disk_gib == 0 {
            return Err("disk must be at least 1 GiB".into());
        }
        if self.width < 320 || self.height < 200 {
            return Err("display size must be at least 320x200".into());
        }
        Ok(())
    }
}

/// A host directory shared into the guest: `HOST:GUEST[:ro]`.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
}

impl Mount {
    /// Relative guest paths are relative to the guest user's home.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (spec, readonly) = match spec.strip_suffix(":ro") {
            Some(spec) => (spec, true),
            None => (spec, false),
        };
        let (host, guest) = spec
            .split_once(':')
            .ok_or_else(|| format!("invalid mount {spec:?}; expected HOST:GUEST[:ro]"))?;
        let host = Path::new(host)
            .canonicalize()
            .map_err(|error| format!("invalid mount source {host}: {error}"))?;
        if !host.is_dir() {
            return Err(format!("mount source {} is not a directory", host.display()));
        }
        if guest.is_empty() {
            return Err(format!("invalid mount {spec:?}; the guest path is empty"));
        }
        let guest = match guest.starts_with('/') {
            true => guest.to_owned(),
            false => format!("/home/varmint/{guest}"),
        };
        Ok(Self { host, guest, readonly })
    }

    pub fn spec(&self) -> String {
        let suffix = if self.readonly { ":ro" } else { "" };
        format!("{}:{}{suffix}", self.host.display(), self.guest)
    }
}

/// The guest variables as a JSON object: from `varmint serve` to `varmint run`,
/// and from `run` to the VM process. Unlike arguments, a process environment
/// is not visible to other users in `ps`, and one variable keeps names chosen
/// by a remote client out of the host processes' own environment.
pub const GUEST_ENVIRONMENT_VARIABLE: &str = "VARMINT_GUEST_ENVIRONMENT";

/// Variables copied into every VM from this process's environment when they
/// are set, without `--env`: an agent's credentials.
pub const PASSED_VARIABLES: [&str; 2] = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"];

/// Variables every guest session gets: each of PASSED_VARIABLES that is set,
/// then those in GUEST_ENVIRONMENT_VARIABLE, then each `--env NAME` with its
/// value from this process's environment.
/// Values never come from arguments, where `ps` and shell history show them.
pub fn guest_environment(names: &[String]) -> Result<BTreeMap<String, String>, String> {
    // @deprecated ~/.varmint/env is no longer read; fail instead of silently
    // starting VMs without the variables it holds.
    let path = instance::home().join("env");
    if path.exists() {
        return Err(format!(
            "{} is no longer read; export its variables in your shell, pass them with --env NAME and delete it",
            path.display()
        ));
    }

    let mut environment = BTreeMap::new();
    for name in PASSED_VARIABLES {
        match env::var(name) {
            Ok(value) => {
                environment.insert(name.to_owned(), value);
            }
            Err(env::VarError::NotPresent) => {}
            Err(error) => return Err(format!("invalid {name}: {error}")),
        }
    }
    match env::var(GUEST_ENVIRONMENT_VARIABLE) {
        Ok(json) => environment.extend(
            serde_json::from_str::<BTreeMap<String, String>>(&json)
                .map_err(|error| format!("invalid {GUEST_ENVIRONMENT_VARIABLE}: {error}"))?,
        ),
        Err(env::VarError::NotPresent) => {}
        Err(error) => return Err(format!("invalid {GUEST_ENVIRONMENT_VARIABLE}: {error}")),
    }
    for name in names {
        if name.contains('=') {
            return Err(format!("--env {name}: give the name only; the value comes from the environment"));
        }
        let value = env::var(name).map_err(|_| format!("--env {name}: {name} is not set here"))?;
        environment.insert(name.clone(), value);
    }

    for (key, value) in &environment {
        let valid_key = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            return Err(format!("invalid environment variable name {key:?}"));
        }
        if value.contains(['\n', '\0']) {
            return Err(format!("the value of {key} must be a single line"));
        }
    }
    Ok(environment)
}

/// A host file or directory copied into the guest at every start: `HOST:GUEST`.
#[derive(Debug, Clone)]
pub struct Seed {
    pub host: PathBuf,
    pub guest: String,
}

impl Seed {
    /// A leading `~/` in HOST is the host home; relative GUEST paths are
    /// relative to the guest user's home.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (host, guest) = spec
            .split_once(':')
            .ok_or_else(|| format!("invalid seed {spec:?}; expected HOST:GUEST"))?;
        let host = match host.strip_prefix("~/") {
            Some(rest) => PathBuf::from(env::var_os("HOME").expect("HOME is not set")).join(rest),
            None => PathBuf::from(host),
        };
        let host = host
            .canonicalize()
            .map_err(|error| format!("invalid seed source {}: {error}", host.display()))?;
        if guest.is_empty() {
            return Err(format!("invalid seed {spec:?}; the guest path is empty"));
        }
        let guest = match guest.starts_with('/') {
            true => guest.to_owned(),
            false => format!("/home/varmint/{guest}"),
        };
        Ok(Self { host, guest })
    }

    pub fn spec(&self) -> String {
        format!("{}:{}", self.host.display(), self.guest)
    }
}

/// Seeds for this run: the `~/.varmint/seeds` directory, copied into the
/// guest home, then `--seed` entries.
pub fn seeds(overrides: &[String]) -> Result<Vec<Seed>, String> {
    let path = instance::home().join("seeds");
    let mut seeds = Vec::new();
    if path.exists() {
        if !path.is_dir() {
            return Err(format!("{} must be a directory; its contents are copied into the guest home", path.display()));
        }
        seeds.push(Seed::parse(&format!("{}:/home/varmint", path.display()))?);
    }
    for spec in overrides {
        seeds.push(Seed::parse(spec)?);
    }
    Ok(seeds)
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub memory_size: usize,
    pub vcpus: usize,
    pub disk: PathBuf,
    pub disk_size: u64,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_args: String,
    pub width: u32,
    pub height: u32,
}

impl VmConfig {
    pub fn new(instance: &Instance, hardware: &Hardware) -> Self {
        let memory_size = usize::try_from(hardware.memory_mib.checked_mul(MIB).expect("memory is too large"))
            .expect("memory is too large for this host");

        Self {
            memory_size,
            vcpus: hardware.cpus,
            disk: instance.disk(),
            disk_size: hardware.disk_gib.checked_mul(GIB).expect("disk is too large"),
            kernel: instance.kernel(),
            initrd: instance.initrd(),
            kernel_args: format!(
                "{} {KERNEL_ARGS} systemd.hostname={}",
                machine::CONSOLE_ARGS,
                instance.name
            ),
            width: hardware.width,
            height: hardware.height,
        }
    }
}

pub struct Resources {
    pub base_image: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
}

impl Resources {
    pub fn locate() -> Self {
        let resources = runtime_dir().map(|runtime| runtime.join("Resources"));
        Self {
            base_image: resource_path(
                resources.as_deref(),
                "runtime/base.raw.zst",
                "build/guest/varmint-debian.raw.zst",
            ),
            kernel: resource_path(resources.as_deref(), "kernel/Image", "build/guest/Image"),
            initrd: resource_path(resources.as_deref(), "kernel/initrd", "build/guest/initrd"),
        }
    }
}

/// Directory holding `Helpers/`, `Frameworks/` and `Resources/`:
/// `PREFIX/share/varmint` for `PREFIX/bin/varmint`. `None` for development
/// builds run from the cargo target directory.
pub fn runtime_dir() -> Option<PathBuf> {
    let executable = env::current_exe().ok()?.canonicalize().ok()?;
    let share = executable.parent()?.parent()?.join("share/varmint");
    share.is_dir().then_some(share)
}

fn resource_path(resources: Option<&Path>, bundled_name: &str, development_path: &str) -> PathBuf {
    resources
        .map(|resources| resources.join(bundled_name))
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(development_path))
}
