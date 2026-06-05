//! File-backed, size-rotating, turn-keyed record store — the flight
//! recorder's durable substrate.
//!
//! Layout under the data dir (default `~/.cache/wintermute/lucid/`):
//!
//! ```text
//! lucid/
//!   segment-<n>.ndjson      one NDJSON record per line; <n> is monotonic
//!   index.ndjson            append-only sidecar: {turn_id, segment, line}
//! ```
//!
//! Persistence model:
//! - Each [`LucidStore::append`] writes one record line to the active
//!   segment and one index line to the sidecar, flushing both to the OS so a
//!   process crash loses at most the in-flight write (AC5).
//! - When the active segment would exceed `max_segment_bytes`, the store
//!   rolls to a fresh segment. Prior segments are NEVER truncated (AC4).
//! - When the number of segments exceeds `max_segments`, the oldest whole
//!   segments are pruned so total on-disk size stays bounded (AC4). The
//!   active segment is never pruned.
//! - On startup the store discovers existing segments, resumes appending to
//!   the highest-numbered one, and rebuilds the in-memory turn index from the
//!   sidecar so records written before a restart remain queryable (AC5).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::record::Record;

const SEGMENT_PREFIX: &str = "segment-";
const SEGMENT_SUFFIX: &str = ".ndjson";
const INDEX_FILE: &str = "index.ndjson";

/// Rotation policy for the store. Defaults bound the recorder to roughly
/// `max_segments * max_segment_bytes` on disk.
#[derive(Debug, Clone, Copy)]
pub struct RotationPolicy {
    /// Roll to a new segment once the active one reaches this many bytes.
    pub max_segment_bytes: u64,
    /// Keep at most this many segments; prune oldest beyond it.
    pub max_segments: usize,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        // ~16 MiB per segment * 16 segments ≈ 256 MiB ceiling.
        Self {
            max_segment_bytes: 16 * 1024 * 1024,
            max_segments: 16,
        }
    }
}

/// One sidecar index entry, locating a record's correlation key.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    key: String,
    segment: u64,
    line: u64,
}

/// A located record position (which segment and which 0-based line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    /// Segment number containing the record.
    pub segment: u64,
    /// 0-based line within the segment.
    pub line: u64,
}

/// The durable record store.
pub struct LucidStore {
    dir: PathBuf,
    policy: RotationPolicy,
    active_segment: u64,
    active_bytes: u64,
    active_lines: u64,
    writer: File,
    index_writer: File,
    /// correlation key -> ordered list of locations.
    index: BTreeMap<String, Vec<RecordLocation>>,
}

impl LucidStore {
    /// The default data directory, honoring `$XDG_CACHE_HOME`.
    #[must_use]
    pub fn default_data_dir() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("wintermute/lucid");
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".cache/wintermute/lucid")
    }

    /// Open (creating if needed) a store rooted at `dir` with the given
    /// rotation policy. Resumes the latest segment and rebuilds the index
    /// from the sidecar.
    ///
    /// # Errors
    /// Returns an error on any filesystem failure while creating directories,
    /// opening segments, or reading the existing index sidecar.
    pub fn open(dir: impl AsRef<Path>, policy: RotationPolicy) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).with_context(|| format!("creating lucid dir {}", dir.display()))?;

        let index = rebuild_index(&dir)?;

        let active_segment = highest_segment(&dir)?.unwrap_or(0);
        let seg_path = segment_path(&dir, active_segment);
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .with_context(|| format!("opening segment {}", seg_path.display()))?;
        let active_bytes = fs::metadata(&seg_path).map(|m| m.len()).unwrap_or(0);
        let active_lines = count_lines(&seg_path)?;

        let index_writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(INDEX_FILE))
            .with_context(|| "opening index sidecar")?;

        Ok(Self {
            dir,
            policy,
            active_segment,
            active_bytes,
            active_lines,
            writer,
            index_writer,
            index,
        })
    }

    /// Append a record durably. Rotates first if the active segment is full,
    /// then writes the record line and its index entry, flushing both.
    ///
    /// # Errors
    /// Returns an error on serialization or any filesystem write failure.
    pub fn append(&mut self, rec: &Record) -> Result<RecordLocation> {
        let line_json = rec.to_ndjson()?;
        let line_bytes = line_json.len() as u64 + 1; // + newline

        // Rotate before writing if this record would overflow the active
        // segment (but always keep at least one record per segment so an
        // oversized single record still lands somewhere).
        if self.active_lines > 0 && self.active_bytes + line_bytes > self.policy.max_segment_bytes {
            self.rotate()?;
        }

        let loc = RecordLocation {
            segment: self.active_segment,
            line: self.active_lines,
        };

        writeln!(self.writer, "{line_json}").context("writing record line")?;
        self.writer.flush().context("flushing segment")?;
        self.active_bytes += line_bytes;
        self.active_lines += 1;

        let key = rec.correlation_key();
        let entry = IndexEntry {
            key: key.clone(),
            segment: loc.segment,
            line: loc.line,
        };
        let entry_json = serde_json::to_string(&entry)?;
        writeln!(self.index_writer, "{entry_json}").context("writing index entry")?;
        self.index_writer.flush().context("flushing index")?;
        self.index.entry(key).or_default().push(loc);

        Ok(loc)
    }

    /// Roll over to a fresh, empty segment without touching prior ones, and
    /// prune the oldest segments past the retention cap.
    fn rotate(&mut self) -> Result<()> {
        let next = self.active_segment + 1;
        let seg_path = segment_path(&self.dir, next);
        self.writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .with_context(|| format!("creating segment {}", seg_path.display()))?;
        self.active_segment = next;
        self.active_bytes = 0;
        self.active_lines = 0;
        self.prune()?;
        Ok(())
    }

    /// Delete the oldest whole segments until at most `max_segments` remain.
    /// Never deletes the active segment.
    fn prune(&mut self) -> Result<()> {
        let mut segs = list_segments(&self.dir)?;
        while segs.len() > self.policy.max_segments {
            let oldest = segs.remove(0);
            if oldest == self.active_segment {
                break;
            }
            let path = segment_path(&self.dir, oldest);
            fs::remove_file(&path)
                .with_context(|| format!("pruning segment {}", path.display()))?;
            // Drop any index entries pointing into the pruned segment.
            for locs in self.index.values_mut() {
                locs.retain(|l| l.segment != oldest);
            }
            self.index.retain(|_, v| !v.is_empty());
        }
        Ok(())
    }

    /// Total record count currently on disk across all live segments.
    ///
    /// # Errors
    /// Returns an error if a segment file cannot be read.
    pub fn record_count(&self) -> Result<u64> {
        let mut total = 0;
        for seg in list_segments(&self.dir)? {
            total += count_lines(&segment_path(&self.dir, seg))?;
        }
        Ok(total)
    }

    /// Number of live segments on disk.
    ///
    /// # Errors
    /// Returns an error if the data directory cannot be read.
    pub fn segment_count(&self) -> Result<usize> {
        Ok(list_segments(&self.dir)?.len())
    }

    /// All records currently on disk, in segment then line order.
    ///
    /// # Errors
    /// Returns an error if a segment cannot be read or a line fails to parse.
    pub fn read_all(&self) -> Result<Vec<Record>> {
        let mut out = Vec::new();
        for seg in list_segments(&self.dir)? {
            for rec in read_segment(&segment_path(&self.dir, seg))? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Locations recorded for a correlation key (from the in-memory index).
    #[must_use]
    pub fn locations_for(&self, key: &str) -> Vec<RecordLocation> {
        self.index.get(key).cloned().unwrap_or_default()
    }

    /// All records recorded under a correlation key, read from disk.
    ///
    /// # Errors
    /// Returns an error if a segment cannot be read.
    pub fn records_for(&self, key: &str) -> Result<Vec<Record>> {
        let mut out = Vec::new();
        for loc in self.locations_for(key) {
            let recs = read_segment(&segment_path(&self.dir, loc.segment))?;
            if let Some(r) = recs.into_iter().nth(loc.line as usize) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// The active segment number (test/introspection aid).
    #[must_use]
    pub const fn active_segment(&self) -> u64 {
        self.active_segment
    }
}

fn segment_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{SEGMENT_PREFIX}{n}{SEGMENT_SUFFIX}"))
}

fn parse_segment_number(name: &str) -> Option<u64> {
    name.strip_prefix(SEGMENT_PREFIX)
        .and_then(|s| s.strip_suffix(SEGMENT_SUFFIX))
        .and_then(|s| s.parse::<u64>().ok())
}

fn list_segments(dir: &Path) -> Result<Vec<u64>> {
    let mut nums = Vec::new();
    if !dir.exists() {
        return Ok(nums);
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(n) = parse_segment_number(name) {
                nums.push(n);
            }
        }
    }
    nums.sort_unstable();
    Ok(nums)
}

fn highest_segment(dir: &Path) -> Result<Option<u64>> {
    Ok(list_segments(dir)?.into_iter().max())
}

fn count_lines(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut n = 0;
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            n += 1;
        }
    }
    Ok(n)
}

fn read_segment(path: &Path) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(Record::from_ndjson(&line)?);
    }
    Ok(out)
}

fn rebuild_index(dir: &Path) -> Result<BTreeMap<String, Vec<RecordLocation>>> {
    let mut index: BTreeMap<String, Vec<RecordLocation>> = BTreeMap::new();
    let path = dir.join(INDEX_FILE);
    if !path.exists() {
        return Ok(index);
    }
    let live: std::collections::BTreeSet<u64> = list_segments(dir)?.into_iter().collect();
    let file = File::open(&path).with_context(|| "opening index sidecar")?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: IndexEntry = serde_json::from_str(&line).context("parsing index entry")?;
        // Skip entries into segments that have since been pruned.
        if live.contains(&entry.segment) {
            index.entry(entry.key).or_default().push(RecordLocation {
                segment: entry.segment,
                line: entry.line,
            });
        }
    }
    Ok(index)
}
