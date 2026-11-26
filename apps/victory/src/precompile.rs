#[cfg(windows)]
mod windows {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context, Result};

    use crate::{Console, shader};

    pub fn prepare(
        cfg: &victory_core::Config,
        exe: &Path,
        title_hash: &str,
        console: Console,
        inputs: Option<&[PathBuf]>,
        jobs: Option<usize>,
    ) -> Result<()> {
        let jobs = jobs
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(2, |cores| (cores.get() / 2).clamp(1, 3))
            })
            .clamp(1, 16);
        let tag = victory_gpu::shader_cache::precompile_setup().map_err(anyhow::Error::msg)?;
        let cache = cfg.cache_subdir("shader-scan")?;
        let paths = match inputs {
            Some(paths) if !paths.is_empty() => paths.to_vec(),
            _ => files(exe)?,
        };
        let seen = Mutex::new(HashSet::new());
        for path in paths {
            scan(&path, &cache, tag, jobs, &seen)?;
        }
        let report = victory_gpu::pipeline::precompile_pipelines(jobs)?;
        println!(
            "pipeline cache    {} ready, {} refused, {} unreadable ({}, {})",
            report.built,
            report.refused,
            report.unreadable,
            console_name(console),
            &title_hash[..12]
        );
        Ok(())
    }

    pub fn record(cfg: &victory_core::Config, build: &str, console: Console) -> Result<()> {
        let dir = cfg.cache_subdir(&format!(
            "pipeline-recipes/{build}/{}",
            console_name(console)
        ))?;
        victory_gpu::pipeline::record_pipeline_recipes(dir)?;
        Ok(())
    }

    fn console_name(console: Console) -> &'static str {
        match console {
            Console::SeriesX => "series-x",
            Console::SeriesS => "series-s",
        }
    }

    fn files(exe: &Path) -> Result<Vec<PathBuf>> {
        let folder = exe
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut paths = vec![exe.to_owned()];
        for entry in walkdir::WalkDir::new(folder) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let ext = entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default();
            if name.ends_with(".dll")
                || name.contains("shader") && name.ends_with(".rpf")
                || ["sgd5", "dxbc", "cdxil", "cso"]
                    .iter()
                    .any(|e| ext.eq_ignore_ascii_case(e))
            {
                paths.push(entry.into_path());
            }
        }
        paths.sort_unstable();
        Ok(paths)
    }

    fn scan(
        path: &Path,
        cache: &Path,
        target: blake3::Hash,
        jobs: usize,
        seen: &Mutex<HashSet<blake3::Hash>>,
    ) -> Result<()> {
        let meta =
            std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            meta.is_file() && meta.len() <= shader::MAX_FILE,
            "unsupported shader input {}",
            path.display()
        );
        let mut key = blake3::Hasher::new();
        key.update(target.as_bytes());
        key.update(std::fs::canonicalize(path)?.as_os_str().as_encoded_bytes());
        key.update(&meta.len().to_le_bytes());
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        if let Some(time) = modified {
            key.update(&time.as_nanos().to_le_bytes());
        }
        let marker = cache.join(key.finalize().to_hex().as_str());
        if modified.is_some()
            && let Ok(result) = std::fs::read_to_string(&marker)
        {
            println!("shader cache      {}  {}", path.display(), result.trim());
            return Ok(());
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sources = shader::sources(&bytes);
        let total = sources.len();
        if total == 0 {
            return Ok(());
        }
        println!("shader archive    {}  {total} records", path.display());
        let next = AtomicUsize::new(0);
        let processed = AtomicUsize::new(0);
        let ready = AtomicUsize::new(0);
        let refused = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..jobs.min(total) {
                scope.spawn(|| {
                    while let Some(source) = sources.get(next.fetch_add(1, Ordering::Relaxed)) {
                        if let Some(bytes) = source.bytes()
                            && let Some(size) = bytes.get(24..28)
                                .map(|word| u32::from_le_bytes(word.try_into().unwrap()) as usize)
                            && let Some(container) = bytes.get(..size)
                        {
                            let hash = blake3::hash(container);
                            if seen.lock().unwrap_or_else(|e| e.into_inner()).insert(hash) {
                                match victory_gpu::shader_cache::precompile_shader(container) {
                                    Ok(()) => { ready.fetch_add(1, Ordering::Relaxed); }
                                    Err(error) => {
                                        refused.fetch_add(1, Ordering::Relaxed);
                                        tracing::debug!(%error, %hash, "shader precompilation refused");
                                    }
                                }
                            }
                        } else {
                            refused.fetch_add(1, Ordering::Relaxed);
                        }
                        let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
                        if done.is_multiple_of(1024) {
                            println!("shader records    {done}/{total} processed");
                        }
                    }
                });
            }
        });
        let result = format!(
            "{} ready, {} refused, {} records",
            ready.into_inner(),
            refused.into_inner(),
            total
        );
        println!("shader cache      {}  {result}", path.display());
        if modified.is_some() {
            let temporary = marker.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&temporary, &result)?;
            std::fs::rename(temporary, marker)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use windows::{prepare, record};
