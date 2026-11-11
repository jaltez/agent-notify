//! GitHub-releases self-update. Port of `internal/update/update.go`.
//!
//! Go builds on `creativeprojects/go-selfupdate` v1.6.0 with a
//! `ChecksumValidator{UniqueFilename: "checksums.txt"}`; the Rust port
//! reimplements exactly the library paths update.go exercises —
//! `DetectLatest` (list the first page of GitHub releases, skip drafts and
//! prereleases, pick the greatest semver with an asset matching
//! `<os>_<arch>` suffixes), the checksums.txt sha256 validation, archive
//! decompression and the in-place binary swap (`update.Apply`) — over
//! `ureq` + `sha2` + `hex` + `flate2`/`tar`/`zip`. The library's own tests
//! (go-selfupdate@v1.6.0 detect.go / validate.go / update/apply.go) are the
//! specification for each step.

use crate::buildinfo::{self, OWNER, REPO};
use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Releases are fetched from the GitHub API, one page, like
/// `githubSource.ListReleases` (go-selfupdate passes no list options, so
/// go-github's default `per_page` of 30 applies).
const API: &str = "https://api.github.com";
const PER_PAGE: usize = 30;
const USER_AGENT: &str = "agent-notify-selfupdate";

/// A published release, as far as the updater needs it
/// (`selfupdate.Release` reduced to what update.go touches, plus the asset
/// fields Apply needs: name for decompression/checksum lookup and the
/// checksums.txt URL for validation).
#[derive(Clone, Debug, Default)]
pub struct Release {
    /// release tag, e.g. "v0.5.0".
    pub version: String,
    /// the asset for the running platform.
    pub asset_url: String,
    /// asset filename, e.g. "agent-notify_linux_amd64.tar.gz"
    /// (`Release.AssetName`).
    pub asset_name: String,
    /// goreleaser checksums.txt browser-download URL
    /// (`Release.ValidationAssetURL`).
    pub validation_asset_url: String,
}

impl Release {
    /// `rel.GreaterThan(other)` (release.go): whether this release's semver
    /// is strictly greater than `other`. Go panics on an unparseable
    /// `other`; this returns false instead (both sides are parsed from
    /// goreleaser tags, so it cannot happen in practice).
    fn greater_than(&self, other: &str) -> bool {
        match (semver(&self.version), semver(other)) {
            (Some(a), Some(b)) => a.cmp(&b) == std::cmp::Ordering::Greater,
            _ => false,
        }
    }
}

/// The outcome of a version check (`update.Status`).
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub current: String,
    pub latest: String,
    pub update_avail: bool,
    /// set when a release was found, for [`Checker::apply`].
    pub release: Option<Release>,
}

impl Status {
    /// Renders a Status for humans (`Status.Describe`, update.go:95).
    pub fn describe(&self) -> String {
        if self.latest == "none" {
            return format!("current {}, no releases published", self.current);
        }
        if !buildinfo::is_release() {
            return format!("development build, latest release is {}", self.latest);
        }
        if self.update_avail {
            return format!("update available: {} → {}", self.current, self.latest);
        }
        format!("up to date ({})", self.current)
    }
}

/// Talks to the GitHub releases source (`update.Checker`).
pub struct Checker;

impl Checker {
    /// Builds a Checker verifying assets against goreleaser's checksums.txt
    /// (`update.New`, update.go:29). The GitHub source construction cannot
    /// fail here, so the Result only mirrors the Go signature.
    pub fn new() -> anyhow::Result<Checker> {
        Ok(Checker)
    }

    /// Compares the running build against the latest release (`Checker.Check`,
    /// update.go:50). Development builds ("dev") never report an update; they
    /// only show the latest published version. `timeout` stands in for the
    /// Go 20s context.
    pub fn check(&self, timeout: Duration) -> anyhow::Result<Status> {
        let mut st = Status::default();
        st.current = buildinfo::VERSION.to_string();
        let rel = self.detect_latest(timeout)?;
        let Some(rel) = rel else {
            st.latest = "none".to_string();
            return Ok(st);
        };
        st.latest = rel.version.clone();
        st.release = Some(rel);
        if !buildinfo::is_release() {
            return Ok(st);
        }
        if let Some(cur) = semver(buildinfo::VERSION) {
            st.update_avail = st
                .release
                .as_ref()
                .expect("release set above")
                .greater_than(&cur.to_string());
        }
        Ok(st)
    }

    /// Downloads the release asset, verifies its checksum and swaps the
    /// running binary in place (on Windows the running exe is renamed
    /// aside) (`Checker.Apply`, update.go:76 → `Updater.UpdateTo`).
    pub fn apply(&self, release: &Release) -> anyhow::Result<()> {
        let exe = std::env::current_exe().context("cannot locate the running binary")?;
        let agent = plain_agent();
        let data = http_get(&agent, &release.asset_url, false)
            .map_err(|e| anyhow::anyhow!("failed to read asset {:?}: {e}", release.asset_name))?;
        let sums = http_get(&agent, &release.validation_asset_url, false).map_err(|e| {
            anyhow::anyhow!("failed reading validation data {:?}: {e}", release.asset_name)
        })?;
        validate_checksum(&release.asset_name, &data, &sums)?;

        let cmd = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bin = decompress_command(&data, &release.asset_name, &cmd, go_os(), go_arch())?;
        apply_binary(&bin, &exe)
    }

    /// `Updater.DetectLatest` (go-selfupdate detect.go): list releases, find
    /// the latest stable one carrying an asset for this platform AND a
    /// checksums.txt validation asset. `Ok(None)` when nothing matched.
    fn detect_latest(&self, timeout: Duration) -> anyhow::Result<Option<Release>> {
        let rels = list_releases(&timed_agent(timeout))?;
        for arch in additional_arches(go_arch()) {
            if let Some((ver, rel, asset)) = find_release_and_asset_for_arch(&rels, &arch) {
                // validateReleaseAsset: the validator's asset must exist in
                // the release, or DetectLatest fails outright.
                let validation = rel
                    .assets
                    .iter()
                    .find(|a| a.name == CHECKSUMS_FILE)
                    .ok_or_else(|| {
                        anyhow::anyhow!("validation file not found: {CHECKSUMS_FILE:?}")
                    })?;
                return Ok(Some(Release {
                    version: ver.to_string(),
                    asset_url: asset.browser_download_url.clone(),
                    asset_name: asset.name.clone(),
                    validation_asset_url: validation.browser_download_url.clone(),
                }));
            }
        }
        Ok(None)
    }
}

/// goreleaser's checksum file name (`.goreleaser.yml` checksum
/// name_template; Go: `ChecksumValidator{UniqueFilename: "checksums.txt"}`).
const CHECKSUMS_FILE: &str = "checksums.txt";

// ---------------------------------------------------------------- GitHub --
// go-github release/asset JSON, reduced to the fields used.

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// `githubSource.ListReleases`: first page of releases; 404 (unknown
/// repo / no releases) is an empty list, other errors propagate.
fn list_releases(agent: &ureq::Agent) -> anyhow::Result<Vec<GhRelease>> {
    let url = format!("{API}/repos/{OWNER}/{REPO}/releases?per_page={PER_PAGE}");
    match agent.get(&url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader().read_to_end(&mut buf)?;
            Ok(serde_json::from_slice(&buf)?)
        }
        Err(ureq::Error::Status(404, _)) => Ok(Vec::new()),
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("GET {url}: http status {code}"))
        }
        Err(e) => Err(anyhow::anyhow!("GET {url}: {e}")),
    }
}

/// go_os / go_arch map Rust's platform constants to Go's names, which is
/// what goreleaser bakes into asset names (`linux_amd64`, `windows_amd64`).
fn go_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS // "linux" and friends match Go verbatim
    }
}

fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

/// `getAdditionalArch` (arch.go): the generic arch first, then more
/// specific/alternative spellings. GOARM probing is not ported (release
/// assets here are amd64-only; PORT-NOTE in the module report).
fn additional_arches(arch: &str) -> Vec<String> {
    let mut v = vec![arch.to_string()];
    if arch == "amd64" {
        v.push("x86_64".to_string());
    }
    if arch == "386" {
        v.extend(["i386", "x86", "x86_32"].into_iter().map(String::from));
    }
    v
}

/// `getSuffixes` (detect.go): every `os<sep>arch<ext>` combination an
/// asset may end with, '_' and '-' separators, archive extensions plus raw.
fn get_suffixes(os: &str, arch: &str) -> Vec<String> {
    let mut out = Vec::new();
    for sep in ['_', '-'] {
        for ext in [".zip", ".tar.gz", ".tgz", ".gzip", ".gz", ".tar.xz", ".xz", ".bz2", ""] {
            out.push(format!("{os}{sep}{arch}{ext}"));
            if os == "windows" {
                out.push(format!("{os}{sep}{arch}.exe{ext}"));
            }
        }
    }
    out
}

/// `findReleaseAndAssetForArch` (detect.go): scan the release list (GitHub
/// returns newest first), skip drafts/prereleases, and keep the release
/// with the strictly greatest semver that carries a matching asset.
fn find_release_and_asset_for_arch<'a>(
    rels: &'a [GhRelease],
    arch: &str,
) -> Option<(SemVer, &'a GhRelease, &'a GhAsset)> {
    let suffixes = get_suffixes(go_os(), arch);
    let mut best: Option<(SemVer, &'a GhRelease, &'a GhAsset)> = None;
    for rel in rels {
        if rel.draft || rel.prerelease {
            continue;
        }
        let Some(ver) = version_from_tag(&rel.tag_name) else {
            continue;
        };
        let Some(asset) = rel.assets.iter().find(|a| {
            let name = a.name.to_lowercase();
            suffixes.iter().any(|s| name.ends_with(s.as_str()))
                || {
                    let url = a.browser_download_url.to_lowercase();
                    suffixes.iter().any(|s| url.ends_with(s.as_str()))
                }
        }) else {
            continue;
        };
        // any version with a suffix is less than one without; strictly
        // greater wins, so the first (newest) release of equal version
        // keeps the asset — same as Go.
        if best.as_ref().is_none_or(|(b, _, _)| ver > *b) {
            best = Some((ver, rel, asset));
        }
    }
    best
}

/// `reVersion.FindStringIndex` + `semver.NewVersion` (detect.go): slice the
/// tag at the first `\d+\.\d+\.\d+` and parse what follows (prerelease and
/// metadata survive).
fn version_from_tag(tag: &str) -> Option<SemVer> {
    let b = tag.as_bytes();
    for i in 0..b.len() {
        if !starts_version(b, i) {
            continue;
        }
        return semver(&tag[i..]);
    }
    None
}

/// Whether tag[i..] begins with `\d+\.\d+\.\d+`.
fn starts_version(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == i || j >= b.len() || b[j] != b'.' {
        return false;
    }
    let mut k = j + 1;
    while k < b.len() && b[k].is_ascii_digit() {
        k += 1;
    }
    if k == j + 1 || k >= b.len() || b[k] != b'.' {
        return false;
    }
    let mut l = k + 1;
    while l < b.len() && b[l].is_ascii_digit() {
        l += 1;
    }
    l > k + 1
}

// ---------------------------------------------------------------- semver --
// Masterminds/semver v3 `NewVersion` (the lenient form: optional leading
// 'v', missing minor/patch coerced to 0), which is what go-selfupdate and
// update.go use for comparison.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreId>,
    pub meta: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreId {
    Num(u64),
    Alpha(String),
}

/// Masterminds-style version parse; `None` when the text does not parse.
pub(crate) fn semver(s: &str) -> Option<SemVer> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let (main, meta) = match s.split_once('+') {
        Some((a, b)) => (a, Some(b.to_string())),
        None => (s, None),
    };
    if let Some(m) = &meta {
        if m.is_empty() || !m.split('.').all(valid_id) {
            return None;
        }
    }
    let (core, pre) = match main.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (main, None),
    };
    let mut parts = core.split('.');
    let mut nums = [0u64; 3];
    for (idx, slot) in nums.iter_mut().enumerate() {
        match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) => {
                *slot = p.parse().ok()?;
            }
            None if idx > 0 => break, // 1.2 / 1 forms: missing parts are 0
            _ => return None,
        }
    }
    if parts.next().is_some() {
        return None; // 1.2.3.4 and beyond are rejected
    }
    let pre = match pre {
        Some(p) if !p.is_empty() => p
            .split('.')
            .map(|id| match id.parse::<u64>() {
                Ok(n) => Ok(PreId::Num(n)),
                _ if valid_id(id) => Ok(PreId::Alpha(id.to_string())),
                _ => Err(()),
            })
            .collect::<Result<Vec<_>, _>>()
            .ok()?,
        Some(_) => return None, // empty pre after '-'
        None => Vec::new(),
    };
    Some(SemVer {
        major: nums[0],
        minor: nums[1],
        patch: nums[2],
        pre,
        meta,
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let ids: Vec<String> = self.pre.iter().map(|p| p.to_string()).collect();
            write!(f, "-{}", ids.join("."))?;
        }
        if let Some(m) = &self.meta {
            write!(f, "+{m}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for PreId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreId::Num(n) => write!(f, "{n}"),
            PreId::Alpha(s) => write!(f, "{s}"),
        }
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Semver precedence: numbers, then prerelease (build metadata ignored;
/// no prerelease > any prerelease; numeric ids < alphanumeric; a longer
/// prerelease wins when its prefix matches).
impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| cmp_pre(&self.pre, &other.pre))
    }
}

fn cmp_pre(a: &[PreId], b: &[PreId]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Equal,
        (true, false) => return Greater,
        (false, true) => return Less,
        _ => {}
    }
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x, y) {
            (PreId::Num(x), PreId::Num(y)) => x.cmp(y),
            (PreId::Num(_), PreId::Alpha(_)) => Less,
            (PreId::Alpha(_), PreId::Num(_)) => Greater,
            (PreId::Alpha(x), PreId::Alpha(y)) => x.cmp(y),
        };
        if ord != Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

// -------------------------------------------------------------- checksum --

/// `SHAValidator` + `ChecksumValidator.Validate` (validate.go): find the
/// asset's line in goreleaser's checksums.txt and byte-compare the sha256.
fn validate_checksum(filename: &str, data: &[u8], sums: &[u8]) -> anyhow::Result<()> {
    let hash = find_checksum(filename, sums)?;
    // SHAValidator: at least sha256.BlockSize (64) bytes, truncated to 64.
    if hash.len() < 64 {
        anyhow::bail!("incorrect checksum file format");
    }
    let hash = &hash[..64];
    let calculated = hex::encode(Sha256::digest(data));
    let equal = hash.len() == 64
        && (hex::decode(hash).ok().zip(hex::decode(&calculated).ok()))
            .is_some_and(|(a, b)| a == b);
    if !equal {
        anyhow::bail!("expected {hash:?}, found {calculated:?}: sha256 validation failed");
    }
    Ok(())
}

/// `findChecksum` (validate.go): CRLF-aware line split, each non-empty
/// line must be exactly `<hash>  <filename>`.
fn find_checksum(filename: &str, content: &[u8]) -> anyhow::Result<String> {
    let eol: &[u8] = if content.windows(2).any(|w| w == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    for line in split_bytes(content, eol) {
        if line.is_empty() {
            continue;
        }
        let parts = split_bytes(line, b"  ");
        if parts.len() != 2 {
            anyhow::bail!("incorrect checksum file format");
        }
        if parts[1] == filename.as_bytes() {
            return Ok(String::from_utf8_lossy(parts[0]).into_owned());
        }
    }
    anyhow::bail!("hash not found in checksum file")
}

/// `bytes.Split` for byte slices (std's `[u8]::split` only takes
/// closure/char patterns).
fn split_bytes<'a>(hay: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut rest = hay;
    if !sep.is_empty() {
        while rest.len() >= sep.len() {
            match (0..=rest.len() - sep.len()).find(|&i| &rest[i..i + sep.len()] == sep) {
                Some(i) => {
                    out.push(&rest[..i]);
                    rest = &rest[i + sep.len()..];
                }
                None => break,
            }
        }
    }
    out.push(rest);
    out
}

// ------------------------------------------------------------ decompress --

/// `DecompressCommand` (decompress.go): extract the command binary from the
/// release asset, dispatching on the asset filename extension. Returns the
/// raw binary bytes. xz/bzip2 assets are rejected — no decoder crate in the
/// dependency set, and .goreleaser.yml only produces .tar.gz/.zip.
fn decompress_command(
    data: &[u8],
    asset_name: &str,
    cmd: &str,
    os: &str,
    arch: &str,
) -> anyhow::Result<Vec<u8>> {
    if asset_name.ends_with(".zip") {
        return unzip(data, cmd, os, arch);
    }
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        return untar_gz(data, cmd, os, arch);
    }
    if asset_name.ends_with(".gz") || asset_name.ends_with(".gzip") {
        return gunzip(data, cmd, os, arch);
    }
    if asset_name.ends_with(".tar.xz") || asset_name.ends_with(".xz") || asset_name.ends_with(".bz2")
    {
        anyhow::bail!("failed to decompress: {asset_name}: compression format not supported");
    }
    Ok(data.to_vec()) // "File is not compressed"
}

fn unzip(data: &[u8], cmd: &str, os: &str, arch: &str) -> anyhow::Result<Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| anyhow::anyhow!("failed to decompress zip file: {e}"))?;
    for i in 0..z.len() {
        let mut file = z
            .by_index(i)
            .map_err(|e| anyhow::anyhow!("failed to decompress zip file: {e}"))?;
        if file.is_dir() {
            continue;
        }
        let name = Path::new(file.name())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if match_executable_name(cmd, os, arch, &name) {
            let mut out = Vec::new();
            file.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("executable not found in zip file: {cmd:?}")
}

fn untar_gz(data: &[u8], cmd: &str, os: &str, arch: &str) -> anyhow::Result<Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut t = tar::Archive::new(gz);
    for entry in t
        .entries()
        .map_err(|e| anyhow::anyhow!("failed to decompress tar file: {e}"))?
    {
        let mut entry =
            entry.map_err(|e| anyhow::anyhow!("failed to decompress tar file: {e}"))?;
        let name = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if match_executable_name(cmd, os, arch, &name) {
            let mut out = Vec::new();
            entry.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    anyhow::bail!("executable not found in tar: {cmd:?}")
}

fn gunzip(data: &[u8], cmd: &str, os: &str, arch: &str) -> anyhow::Result<Vec<u8>> {
    let mut gz = flate2::read::GzDecoder::new(data);
    let name = gz
        .header()
        .and_then(|h| h.filename())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_default();
    if !match_executable_name(cmd, os, arch, &name) {
        anyhow::bail!("executable not found: expected {cmd:?} but found {name:?}");
    }
    let mut out = Vec::new();
    gz.read_to_end(&mut out)?;
    Ok(out)
}

/// `matchExecutableName` (decompress.go): `^cmd([_-]v?SEMVER)?([_-]os[_-]arch)?(\.exe)?$`
/// without a regex engine — same grammar, resolved by stripping the
/// os-arch suffix first, then parsing the version remainder.
fn match_executable_name(cmd: &str, os: &str, arch: &str, target: &str) -> bool {
    let cmd = cmd.strip_suffix(".exe").unwrap_or(cmd);
    let target = target.strip_suffix(".exe").unwrap_or(target);
    let Some(rest) = target.strip_prefix(cmd) else {
        return false;
    };
    // optional `[_-]os[_-]arch` group at the end
    let head = strip_os_arch(rest, os, arch).unwrap_or(rest);
    if head.is_empty() {
        return true;
    }
    // head must be `[_-]v?SEMVER`
    let Some(body) = head.get(1..) else {
        return false;
    };
    if !matches!(head.as_bytes()[0], b'_' | b'-') {
        return false;
    }
    let body = body.strip_prefix('v').unwrap_or(body);
    semver(body).is_some()
}

/// Strips a trailing `[_-]os[_-]arch` group, returning what precedes it —
/// but only when that remainder is empty or itself starts with a separator
/// (i.e. is a version group), so "linuxamd64" garbage does not half-match.
fn strip_os_arch<'a>(rest: &'a str, os: &str, arch: &str) -> Option<&'a str> {
    for s1 in ['_', '-'] {
        for s2 in ['_', '-'] {
            let g2 = format!("{s1}{os}{s2}{arch}");
            if let Some(head) = rest.strip_suffix(&g2) {
                if head.is_empty() || (head.len() > 1 && matches!(head.as_bytes()[0], b'_' | b'-'))
                {
                    return Some(head);
                }
            }
        }
    }
    None
}

// ------------------------------------------------------------------ swap --
// `update.Apply` (go-selfupdate update/apply.go): write `<dir>/.<name>.new`,
// rename target → `<dir>/.<name>.old`, rename the new file in, then remove
// (or, on Windows, hide) the old one; roll back if the second rename fails.

fn apply_binary(new_bytes: &[u8], target: &Path) -> anyhow::Result<()> {
    let dir: PathBuf = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let filename = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("invalid executable path {}", target.display()))?;

    let new_path = dir.join(format!(".{filename}.new"));
    write_mode(&new_path, new_bytes, 0o755)?;

    let old_path = dir.join(format!(".{filename}.old"));
    // delete any existing old file: on Windows renames fail onto an
    // existing destination, and after a successful update the running
    // process cannot delete its own .old file.
    let _ = std::fs::remove_file(&old_path);

    std::fs::rename(target, &old_path)
        .map_err(|e| anyhow::anyhow!("rename {} to {}: {e}", target.display(), old_path.display()))?;

    match std::fs::rename(&new_path, target) {
        Ok(()) => {
            if std::fs::remove_file(&old_path).is_err() {
                // windows cannot delete the binary it is running: hide it.
                #[cfg(target_os = "windows")]
                hide_file(&old_path);
            }
            Ok(())
        }
        Err(err) => {
            // bad state: old binary moved away, new one did not land. Try
            // to roll back.
            match std::fs::rename(&old_path, target) {
                Ok(()) => Err(anyhow::anyhow!(
                    "rename {} to {}: {err}",
                    new_path.display(),
                    target.display()
                )),
                Err(rerr) => Err(anyhow::anyhow!(
                    "update failed ({err}); rollback also failed ({rerr}): no executable at {}",
                    target.display()
                )),
            }
        }
    }
}

fn write_mode(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, data)
    }
}

/// `hideFile` (go-selfupdate update/hide_windows.go): windows cannot delete
/// the binary it is running, so the .old leftover is hidden instead.
#[cfg(target_os = "windows")]
fn hide_file(path: &Path) {
    use ::windows::core::HSTRING;
    use ::windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    let s = path.to_string_lossy();
    let _ = unsafe { SetFileAttributesW(&HSTRING::from(s.as_ref()), FILE_ATTRIBUTE_HIDDEN) };
}

// ------------------------------------------------------------------- http --

/// Agent for the release listing with the Go `context.WithTimeout` equivalent.
fn timed_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

/// Agent for asset downloads (no overall timeout, like the unbounded
/// context callers pass to Apply).
fn plain_agent() -> ureq::Agent {
    ureq::Agent::new()
}

/// GET bytes. `github_api` requests get the JSON accept header and the
/// optional GITHUB_TOKEN; asset downloads go through the public
/// browser_download_url, without credentials (go-github follows GitHub's
/// redirect to the object store, where the auth header is dropped anyway).
fn http_get(agent: &ureq::Agent, url: &str, github_api: bool) -> anyhow::Result<Vec<u8>> {
    let mut req = agent.get(url).set("User-Agent", USER_AGENT);
    if github_api {
        req = req.set("Accept", "application/vnd.github+json");
        if let Ok(tok) = std::env::var("GITHUB_TOKEN") {
            if !tok.is_empty() {
                req = req.set("Authorization", &format!("Bearer {tok}"));
            }
        }
    }
    match req.call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader().read_to_end(&mut buf)?;
            Ok(buf)
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(anyhow::anyhow!("GET {url}: http status {code}"))
        }
        Err(e) => Err(anyhow::anyhow!("GET {url}: {e}")),
    }
}

/// Launches the (already swapped) binary as a detached process and returns;
/// the caller should exit right after (`update.RestartSelf`, update.go:86).
pub fn restart_self() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).spawn()?;
    Ok(())
}

// ------------------------------------------------------------------ tests --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parses_masterminds_lenient_forms() {
        assert_eq!(semver("0.5.0").unwrap().to_string(), "0.5.0");
        assert_eq!(semver("v0.5.0").unwrap().to_string(), "0.5.0");
        assert_eq!(semver("1.2").unwrap().to_string(), "1.2.0");
        assert_eq!(semver("3").unwrap().to_string(), "3.0.0");
        assert_eq!(
            semver("0.5.0-rc.1").unwrap().to_string(),
            "0.5.0-rc.1"
        );
        assert!(semver("").is_none());
        assert!(semver("abc").is_none());
        assert!(semver("1.2.3.4").is_none());
        assert!(semver("1.2.3-").is_none()); // empty prerelease
    }

    #[test]
    fn semver_precedence_matches_semver_spec() {
        let gt = |a: &str, b: &str| semver(a).unwrap().cmp(&semver(b).unwrap());
        assert_eq!(gt("0.5.1", "0.5.0"), std::cmp::Ordering::Greater);
        assert_eq!(gt("0.10.0", "0.9.0"), std::cmp::Ordering::Greater);
        assert_eq!(gt("1.0.0", "1.0.0-rc.1"), std::cmp::Ordering::Greater);
        assert_eq!(gt("1.0.0-rc.2", "1.0.0-rc.1"), std::cmp::Ordering::Greater);
        // numeric identifiers compare numerically, and are less than alpha
        assert_eq!(gt("1.0.0-2", "1.0.0-1"), std::cmp::Ordering::Greater);
        assert_eq!(gt("1.0.0-alpha", "1.0.0-1"), std::cmp::Ordering::Greater);
        // longer prerelease wins on equal prefix
        assert_eq!(gt("1.0.0-alpha.1", "1.0.0-alpha"), std::cmp::Ordering::Greater);
        // build metadata is ignored
        assert_eq!(
            gt("1.0.0+build.1", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn tag_versions_slice_at_the_first_semver() {
        assert_eq!(version_from_tag("v0.5.0").unwrap().to_string(), "0.5.0");
        assert_eq!(version_from_tag("release-1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(
            version_from_tag("v0.5.0-rc1").unwrap().to_string(),
            "0.5.0-rc1"
        );
        assert_eq!(
            version_from_tag("app-v2.10.11+x").unwrap().to_string(),
            "2.10.11+x"
        );
        assert!(version_from_tag("nope").is_none());
        assert!(version_from_tag("1.2").is_none()); // needs \d+\.\d+\.\d+
    }

    #[test]
    fn release_greater_than_uses_semver() {
        let rel = Release {
            version: "0.5.0".into(),
            ..Default::default()
        };
        assert!(rel.greater_than("0.4.9"));
        assert!(rel.greater_than("0.4"));
        assert!(!rel.greater_than("0.5.0"));
        assert!(!rel.greater_than("garbage")); // Go would panic; we return false
    }

    fn gh_release(tag: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|name| GhAsset {
                    name: name.to_string(),
                    browser_download_url: format!("https://github.com/x/y/releases/download/t/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn suffixes_match_goreleaser_assets() {
        let rels = vec![
            gh_release("v0.4.0", &["agent-notify_0.4.0_checksums.txt", "README.md"]),
            gh_release(
                "v0.5.0",
                &[
                    "checksums.txt",
                    "agent-notify_linux_amd64.tar.gz",
                    "agent-notify_windows_amd64.zip",
                ],
            ),
        ];
        let found = find_release_and_asset_for_arch(&rels, "amd64");
        let Some((ver, rel, asset)) = found else {
            panic!("no asset found");
        };
        assert_eq!(ver.to_string(), "0.5.0");
        assert_eq!(rel.tag_name, "v0.5.0");
        assert_eq!(asset.name, "agent-notify_linux_amd64.tar.gz");
    }

    #[test]
    fn drafts_prereleases_and_unrelated_assets_are_skipped() {
        let mut rc = gh_release("v0.6.0", &["agent-notify_linux_amd64.tar.gz", "checksums.txt"]);
        rc.prerelease = true;
        let mut rd = gh_release("v0.7.0", &["agent-notify_linux_amd64.tar.gz", "checksums.txt"]);
        rd.draft = true;
        let rels = vec![
            rc,
            rd,
            gh_release("v0.5.0", &["agent-notify_linux_amd64.tar.gz", "checksums.txt"]),
        ];
        let (ver, rel, _) = find_release_and_asset_for_arch(&rels, "amd64").unwrap();
        assert_eq!(ver.to_string(), "0.5.0");
        assert_eq!(rel.tag_name, "v0.5.0");
    }

    #[test]
    fn checksum_file_parsing() {
        let content = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  agent-notify_linux_amd64.tar.gz\n\
                        ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff  agent-notify_windows_amd64.zip\n";
        assert_eq!(
            find_checksum("agent-notify_windows_amd64.zip", content).unwrap(),
            "f".repeat(64)
        );
        assert_eq!(
            find_checksum("agent-notify_linux_amd64.tar.gz", content).unwrap(),
            "0123456789abcdef".repeat(4)
        );
        // CRLF flavour
        let crlf = content
            .iter()
            .flat_map(|&b| if b == b'\n' { vec![b'\r', b'\n'] } else { vec![b] })
            .collect::<Vec<u8>>();
        assert!(find_checksum("agent-notify_windows_amd64.zip", &crlf).is_ok());
        // malformed line (single space) is rejected
        let bad = b"hash filename\n";
        assert!(find_checksum("x", bad).is_err());
        // missing file
        assert!(find_checksum("nope.tar.gz", content).is_err());
    }

    #[test]
    fn checksum_validation_end_to_end() {
        let data = b"the release payload";
        let sum = hex::encode(Sha256::digest(data));
        let sums = format!("{sum}  agent-notify_linux_amd64.tar.gz\nother  other.zip\n");
        validate_checksum("agent-notify_linux_amd64.tar.gz", data, sums.as_bytes()).unwrap();
        // corrupt payload fails
        assert!(validate_checksum(
            "agent-notify_linux_amd64.tar.gz",
            b"tampered",
            sums.as_bytes()
        )
        .is_err());
        // asset absent from the checksum file fails
        assert!(validate_checksum("other.zip", data, sums.as_bytes()).is_err());
    }

    #[test]
    fn executable_name_matcher_covers_the_grammar() {
        // exact name (the shape this repo's goreleaser archives use)
        assert!(match_executable_name("agent-notify", "linux", "amd64", "agent-notify"));
        assert!(match_executable_name("agent-notify.exe", "windows", "amd64", "agent-notify.exe"));
        assert!(match_executable_name("agent-notify.exe", "windows", "amd64", "agent-notify"));
        // with version and os/arch suffixes
        assert!(match_executable_name("agent-notify", "linux", "amd64", "agent-notify_0.5.0_linux_amd64"));
        assert!(match_executable_name("agent-notify", "linux", "amd64", "agent-notify-v0.5.0-linux-amd64"));
        assert!(match_executable_name("agent-notify", "linux", "amd64", "agent-notify_0.5.0"));
        assert!(match_executable_name("agent-notify", "linux", "amd64", "agent-notify_linux_amd64.exe"));
        // negatives
        assert!(!match_executable_name("agent-notify", "linux", "amd64", "agent-notifyx"));
        assert!(!match_executable_name("agent-notify", "linux", "amd64", "other"));
        assert!(!match_executable_name("agent-notify", "linux", "amd64", "agent-notify_linux_amd64x"));
        assert!(!match_executable_name("agent-notify", "linux", "amd64", "agent-notify_notaversion"));
    }

    #[test]
    fn decompression_finds_the_binary() {
        // raw asset
        assert_eq!(
            decompress_command(b"bin", "agent-notify", "agent-notify", "linux", "amd64").unwrap(),
            b"bin".to_vec()
        );
        // tar.gz with the goreleaser layout
        let mut tarball = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            b.append_data(&mut h, "agent-notify", b"bin".as_slice()).unwrap();
            let mut other = tar::Header::new_gnu();
            other.set_size(5);
            other.set_mode(0o644);
            b.append_data(&mut other, "README.md", b"hello".as_slice()).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(
            decompress_command(&tarball, "agent-notify_linux_amd64.tar.gz", "agent-notify", "linux", "amd64")
                .unwrap(),
            b"bin".to_vec()
        );
        // binary missing from the archive
        assert!(decompress_command(&tarball, "agent-notify_linux_amd64.tar.gz", "other-bin", "linux", "amd64").is_err());
    }

    #[test]
    fn swap_replaces_target_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("an-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("agent-notify");
        std::fs::write(&target, b"old binary").unwrap();
        apply_binary(b"new binary", &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        // old and new temp files are gone
        assert!(!dir.join(".agent-notify.old").exists());
        assert!(!dir.join(".agent-notify.new").exists());
        // second swap over an existing .old file also works
        apply_binary(b"newer binary", &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"newer binary");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn describe_matches_go_strings() {
        let st = |current: &str, latest: &str, avail: bool| Status {
            current: current.to_string(),
            latest: latest.to_string(),
            update_avail: avail,
            release: None,
        };
        assert_eq!(
            st("dev", "none", false).describe(),
            "current dev, no releases published"
        );
        // the last two branches require a release build: a plain build
        // carries VERSION="dev" and Describe reports it as such (same as Go).
        if buildinfo::is_release() {
            assert_eq!(
                st("0.4.0", "0.5.0", true).describe(),
                "update available: 0.4.0 → 0.5.0"
            );
            assert_eq!(st("0.5.0", "0.5.0", false).describe(), "up to date (0.5.0)");
        } else {
            assert_eq!(
                st("0.4.0", "0.5.0", true).describe(),
                "development build, latest release is 0.5.0"
            );
        }
    }
}
