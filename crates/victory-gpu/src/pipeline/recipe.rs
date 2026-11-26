use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};

use super::host::GraphicsState;
use super::make::{self, Pending};

static DIR: OnceLock<PathBuf> = OnceLock::new();
static PREPARED: OnceLock<Mutex<HashMap<String, Prepared>>> = OnceLock::new();
static REUSED: AtomicU64 = AtomicU64::new(0);
static FILE_ID: AtomicU64 = AtomicU64::new(0);

const STAGES: [&str; 8] = [
    "compute",
    "vertex",
    "pixel",
    "mesh",
    "amplification",
    "geometry",
    "domain",
    "hull",
];
const MAX_SHADER: u64 = 16 << 20;
const MAX_RECIPE: u64 = 64 << 10;

pub(super) fn configure(dir: PathBuf) -> io::Result<()> {
    std::fs::create_dir_all(dir.join("shaders"))?;
    std::fs::create_dir_all(dir.join("states"))?;
    let _ = DIR.set(dir);
    Ok(())
}

pub(super) fn take(key: &str) -> Option<Arc<Pending>> {
    let mut cache = PREPARED.get()?.lock().unwrap_or_else(|e| e.into_inner());
    let state = cache.get_mut(key)?;
    let pipeline = state.waiting.take().or_else(|| state.active.upgrade())?;
    let reused = REUSED.fetch_add(1, Ordering::Relaxed) + 1;
    if reused.is_power_of_two() {
        tracing::info!(
            reused,
            "precompiled pipeline states matched at title creation"
        );
    }
    Some(pipeline)
}

struct Prepared {
    waiting: Option<Arc<Pending>>,
    active: Weak<Pending>,
}

enum Work {
    Save(Arc<Pending>),
    Flush(mpsc::Sender<()>),
}

fn recorder() -> Option<&'static mpsc::Sender<Work>> {
    static WRITER: OnceLock<Option<mpsc::Sender<Work>>> = OnceLock::new();
    let dir = DIR.get()?;
    WRITER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let result = std::thread::Builder::new().name("victory-pipeline-recipes".into()).spawn(move || {
            for work in rx {
                match work {
                    Work::Save(pipeline) => {
                        if let Err(error) = save(dir, &pipeline) {
                            tracing::warn!(%error, key = pipeline.key(), "pipeline recipe not saved");
                        }
                    }
                    Work::Flush(done) => { let _ = done.send(()); }
                }
            }
        });
        match result {
            Ok(_) => Some(tx),
            Err(error) => {
                tracing::warn!(%error, "pipeline recipe writer could not start");
                None
            }
        }
    }).as_ref()
}

pub(super) fn record(p: &Arc<Pending>) {
    if let Some(writer) = recorder() {
        let _ = writer.send(Work::Save(p.clone()));
    }
}

pub(super) fn flush() {
    if let Some(writer) = recorder() {
        let (tx, rx) = mpsc::channel();
        if writer.send(Work::Flush(tx)).is_ok() {
            let _ = rx.recv();
        }
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let serial = FILE_ID.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), serial));
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    if path.exists() { Ok(()) } else { result }
}

fn save(dir: &Path, pending: &Pending) -> io::Result<()> {
    let state = dir.join("states").join(pending.key());
    let shaders = pending.shaders();
    if shaders.is_empty() || shaders.len() > STAGES.len() || state.exists() {
        return Ok(());
    }
    let mut bytes = b"VPR1".to_vec();
    bytes.push(shaders.len() as u8);
    for shader in shaders {
        let Some(stage) = STAGES.iter().position(|stage| *stage == shader.stage) else {
            return Ok(());
        };
        if shader.container.len() as u64 > MAX_SHADER {
            return Ok(());
        }
        bytes.push(stage as u8);
        let hash = blake3::hash(&shader.container);
        write_new(
            &dir.join("shaders").join(hash.to_hex().as_str()),
            &shader.container,
        )?;
        bytes.extend(hash.as_bytes());
    }
    if let Some(graphics) = &pending.graphics {
        bytes.extend(graphics.encode_recipe());
    }
    if bytes.len() as u64 > MAX_RECIPE {
        return Ok(());
    }
    write_new(&state, &bytes)
}

fn small_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::other("cache file is too large"));
    }
    Ok(bytes)
}

fn load(dir: &Path, path: &Path) -> anyhow::Result<Arc<Pending>> {
    let bytes = small_file(path, MAX_RECIPE)?;
    let bytes = bytes
        .strip_prefix(b"VPR1")
        .ok_or_else(|| anyhow::anyhow!("invalid recipe header"))?;
    let (&count, bytes) = bytes
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("missing shader count"))?;
    anyhow::ensure!(
        count != 0 && usize::from(count) <= STAGES.len(),
        "invalid shader count"
    );
    let (stages, graphics) = bytes
        .split_at_checked(usize::from(count) * 33)
        .ok_or_else(|| anyhow::anyhow!("truncated recipe"))?;
    let graphics = if graphics.is_empty() {
        None
    } else {
        Some(GraphicsState::decode_recipe(graphics).map_err(anyhow::Error::msg)?)
    };
    let mut shaders = Vec::with_capacity(usize::from(count));
    for record in stages.chunks_exact(33) {
        let stage = *STAGES
            .get(usize::from(record[0]))
            .ok_or_else(|| anyhow::anyhow!("invalid shader stage"))?;
        let hash = blake3::Hash::from_bytes(record[1..].try_into()?);
        let container = small_file(
            &dir.join("shaders").join(hash.to_hex().as_str()),
            MAX_SHADER,
        )?;
        anyhow::ensure!(
            blake3::hash(&container) == hash,
            "shader does not match recipe"
        );
        shaders.push(super::shader(stage, &container));
    }
    let pending = Pending::new(shaders, graphics);
    anyhow::ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some(pending.key()),
        "recipe does not match its name"
    );
    Ok(Arc::new(pending))
}

#[derive(Default, Debug)]
pub struct Report {
    pub built: usize,
    pub refused: usize,
    pub unreadable: usize,
}

pub(super) fn precompile(jobs: usize) -> anyhow::Result<Report> {
    let dir = DIR
        .get()
        .ok_or_else(|| anyhow::anyhow!("pipeline recipe cache is not configured"))?;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir.join("states"))? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            files.push(entry.path());
        }
    }
    files.sort_unstable();
    let mut report = Report::default();
    let total = files.len();
    println!("pipeline recipes  {total}");
    let mut ready = HashMap::new();
    for (index, batch) in files.chunks(128).enumerate() {
        let prepared = Mutex::new(Vec::new());
        let remaining = Mutex::new(batch.iter());
        std::thread::scope(|scope| {
            for _ in 0..jobs.min(batch.len()) {
                scope.spawn(|| {
                    loop {
                        let Some(file) = remaining.lock().unwrap_or_else(|e| e.into_inner()).next()
                        else {
                            break;
                        };
                        let result = load(dir, file).map(|pipeline| {
                            let built = make::now(&pipeline);
                            (pipeline, built)
                        });
                        prepared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(result);
                    }
                });
            }
        });
        for result in prepared.into_inner().unwrap_or_else(|e| e.into_inner()) {
            match result {
                Ok((pipeline, built)) => {
                    if built {
                        report.built += 1;
                    } else {
                        report.refused += 1;
                    }
                    ready.insert(
                        pipeline.key().to_owned(),
                        Prepared {
                            active: Arc::downgrade(&pipeline),
                            waiting: Some(pipeline),
                        },
                    );
                }
                Err(error) => {
                    report.unreadable += 1;
                    tracing::warn!(%error, "pipeline recipe could not be loaded");
                }
            }
        }
        super::library::save(false);
        if index % 4 == 0 || (index + 1) * 128 >= total {
            println!(
                "pipeline states   {}/{total} processed",
                (index * 128 + batch.len()).min(total)
            );
        }
    }
    super::library::save(true);
    let _ = PREPARED.set(Mutex::new(ready));
    Ok(report)
}
