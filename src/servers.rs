//! Resolution of channels against several servers.
//!
//! The primary server (`JULIAUP_SERVER`, or the official one) keeps its cache
//! at `paths.versiondb` exactly as before. Every server added with
//! `juliaup server add` gets a raw cache of its own database under
//! `paths.serversdir`, and [`merge_version_dbs`] presents them as one database
//! in which the first server to define a channel or version wins. Entries from
//! added servers carry absolute `UrlPath`s, which `Url::join` passes through
//! unchanged, so nothing downstream needs to know where a version came from.

use crate::config_file::{JuliaupConfig, JuliaupConfigServer};
use crate::get_juliaup_target;
use crate::global_paths::GlobalPaths;
use crate::jsonstructs_versionsdb::{JuliaupVersionDB, JuliaupVersionDBVersion};
use crate::operations::{download_juliaup_version, download_versiondb};
use crate::utils::{get_juliaserver_base_url, parse_server_url, retry_rename};
use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use std::path::PathBuf;
use url::Url;

/// One server in resolution order.
pub struct ServerEntry {
    pub name: Option<String>,
    pub url: Url,
    /// The primary server: `JULIAUP_SERVER` or the official one.
    pub primary: bool,
}

impl ServerEntry {
    pub fn display_name(&self) -> String {
        match (&self.name, self.primary) {
            (Some(name), _) => name.clone(),
            (None, true) => "primary".to_string(),
            (None, false) => self.url.to_string(),
        }
    }
}

/// The servers channels are resolved against, in order: added servers marked
/// to go first, the primary, then the remaining added servers. An entry whose
/// URL no longer parses is skipped with a warning rather than making every
/// command fail.
pub fn effective_servers(config: &JuliaupConfig) -> Result<Vec<ServerEntry>> {
    let primary = ServerEntry {
        name: None,
        url: get_juliaserver_base_url()?,
        primary: true,
    };

    let mut before = Vec::new();
    let mut after = Vec::new();
    for server in &config.servers {
        match parse_server_url(&server.url) {
            Ok(url) => {
                let entry = ServerEntry {
                    name: server.name.clone(),
                    url,
                    primary: false,
                };
                if server.before_primary {
                    before.push(entry);
                } else {
                    after.push(entry);
                }
            }
            Err(err) => eprintln!(
                "Warning: ignoring configured server `{}`: {}",
                server.url, err
            ),
        }
    }

    let mut out = before;
    out.push(primary);
    out.extend(after);
    Ok(out)
}

/// A filesystem-safe identifier for a server, from its host, port and path:
/// `https://internal.juliahub.com/juliabin/` becomes
/// `internal.juliahub.com_juliabin`. Underscores in the input are doubled so
/// that paths differing only in `_` versus `/` get distinct identifiers.
pub fn server_cache_id(url: &Url) -> String {
    let mut raw = String::new();
    if let Some(host) = url.host_str() {
        raw.push_str(host);
    }
    if let Some(port) = url.port() {
        raw.push('/');
        raw.push_str(&port.to_string());
    }
    let path = url.path().trim_matches('/');
    if !path.is_empty() {
        raw.push('/');
        raw.push_str(path);
    }

    let mut id = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '_' => id.push_str("__"),
            c if c.is_ascii_alphanumeric() || c == '.' || c == '-' => id.push(c),
            _ => id.push('_'),
        }
    }
    id
}

/// Where the raw database of an added server is cached for this target.
pub fn server_versiondb_path(paths: &GlobalPaths, url: &Url) -> PathBuf {
    paths
        .serversdir
        .join(server_cache_id(url))
        .join(format!("versiondb-{}.json", get_juliaup_target()))
}

/// Reads a cached database, or `None` if there is none or it does not parse.
pub fn load_server_versiondb(path: &std::path::Path) -> Option<JuliaupVersionDB> {
    let file = std::fs::File::open(path).ok()?;
    serde_json::from_reader(std::io::BufReader::new(file)).ok()
}

/// What refreshing a server's cache did.
pub enum Refresh {
    /// A newer database was downloaded.
    Downloaded(Version),
    /// The cache was already at the server's version.
    UpToDate(Version),
}

/// Refreshes the cached database of an added server: reads the server's
/// version pointer and downloads its database when that is newer than what
/// is cached. The comparison is with this server's own cache only; the
/// bundled database is not involved, since it comes from the primary.
pub fn refresh_server_versiondb(
    paths: &GlobalPaths,
    url: &Url,
    dbversion_url_path: &str,
) -> Result<Refresh> {
    let pointer_url = url.join(dbversion_url_path).with_context(|| {
        format!("Failed to construct a URL from `{url}` and `{dbversion_url_path}`.")
    })?;
    let online = download_juliaup_version(pointer_url.as_ref())
        .with_context(|| format!("Failed to read the version database version from `{url}`."))?;

    let cache_path = server_versiondb_path(paths, url);
    let cached = load_server_versiondb(&cache_path).and_then(|db| Version::parse(&db.version).ok());
    if let Some(cached) = cached {
        if online <= cached {
            return Ok(Refresh::UpToDate(cached));
        }
    }

    let db_url = url
        .join(&format!(
            "juliaup/versiondb/versiondb-{}-{}.json",
            online,
            get_juliaup_target()
        ))
        .with_context(|| "Failed to construct the version database URL.")?;

    let cache_dir = cache_path
        .parent()
        .ok_or_else(|| anyhow!("Cache path `{}` has no parent.", cache_path.display()))?;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("Failed to create `{}`.", cache_dir.display()))?;
    let temp = tempfile::NamedTempFile::new_in(cache_dir)
        .with_context(|| {
            format!(
                "Failed to create a temporary file in `{}`.",
                cache_dir.display()
            )
        })?
        .into_temp_path();
    download_versiondb(db_url.as_ref(), &temp)
        .with_context(|| format!("Failed to download the version database from `{db_url}`."))?;

    let downloaded = load_server_versiondb(&temp)
        .ok_or_else(|| anyhow!("`{db_url}` is not a juliaup version database."))?;
    if Version::parse(&downloaded.version).ok() != Some(online.clone()) {
        bail!(
            "`{db_url}` declares version `{}`, but the server's pointer says `{online}`.",
            downloaded.version
        );
    }

    retry_rename(&temp, &cache_path)?;
    Ok(Refresh::Downloaded(online))
}

/// The version pointer an added server is asked for. Added servers are asked
/// for the release channel's pointer regardless of the juliaup self-update
/// channel: the distinction concerns juliaup's own builds, not Julia's.
pub const SERVER_DBVERSION_PATH: &str = "juliaup/RELEASECHANNELDBVERSION";

/// Merges databases in resolution order: the first to define a channel or a
/// version wins. Entries from servers given with a base URL get an absolute
/// `UrlPath` against it; the primary is passed without one and keeps its
/// paths relative, so the download step joins them to `JULIAUP_SERVER` as it
/// always has. The merged database carries the primary's version.
pub fn merge_version_dbs(dbs: Vec<(Option<&Url>, JuliaupVersionDB)>) -> JuliaupVersionDB {
    let mut merged = JuliaupVersionDB {
        version: dbs
            .iter()
            .find(|(base, _)| base.is_none())
            .map(|(_, db)| db.version.clone())
            .unwrap_or_default(),
        available_versions: Default::default(),
        available_channels: Default::default(),
    };

    for (base, db) in dbs {
        for (key, version) in db.available_versions {
            merged
                .available_versions
                .entry(key)
                .or_insert_with(|| match base {
                    Some(base) => JuliaupVersionDBVersion {
                        url_path: base
                            .join(&version.url_path)
                            .map(|u| u.to_string())
                            .unwrap_or(version.url_path),
                    },
                    None => version,
                });
        }
        for (name, channel) in db.available_channels {
            merged.available_channels.entry(name).or_insert(channel);
        }
    }
    merged
}

/// Loads the caches of every server in resolution order and merges them
/// around the given primary database.
pub fn merged_versions_db(
    paths: &GlobalPaths,
    config: &JuliaupConfig,
    primary: JuliaupVersionDB,
) -> Result<JuliaupVersionDB> {
    if config.servers.is_empty() {
        return Ok(primary);
    }
    let servers = effective_servers(config)?;
    let mut primary = Some(primary);
    let mut dbs = Vec::with_capacity(servers.len());
    for server in &servers {
        if server.primary {
            dbs.push((None, primary.take().expect("one primary")));
        } else if let Some(db) = load_server_versiondb(&server_versiondb_path(paths, &server.url)) {
            dbs.push((Some(&server.url), db));
        }
    }
    Ok(merge_version_dbs(dbs))
}

/// Parses a server given on the command line: a URL as `JULIAUP_SERVER`
/// accepts, or a bare host such as `julia.example.com`, taken as HTTPS.
pub fn server_url_from_arg(value: &str) -> Result<Url> {
    let value = value.trim();
    if value.contains("://") {
        parse_server_url(value)
    } else {
        parse_server_url(&format!("https://{value}"))
    }
}

/// Finds a configured server by its name or URL.
pub fn find_server(config: &JuliaupConfig, needle: &str) -> Option<usize> {
    let wanted_url = server_url_from_arg(needle).ok();
    config.servers.iter().position(|server| {
        server.name.as_deref() == Some(needle)
            || wanted_url
                .as_ref()
                .is_some_and(|u| parse_server_url(&server.url).ok().as_ref() == Some(u))
    })
}

impl JuliaupConfigServer {
    pub fn new(url: &Url, name: Option<String>, before_primary: bool) -> Self {
        JuliaupConfigServer {
            url: url.to_string(),
            name,
            before_primary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn db(version: &str, versions: &[(&str, &str)], channels: &[(&str, &str)]) -> JuliaupVersionDB {
        JuliaupVersionDB {
            version: version.to_string(),
            available_versions: versions
                .iter()
                .map(|(k, p)| {
                    (
                        k.to_string(),
                        JuliaupVersionDBVersion {
                            url_path: p.to_string(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
            available_channels: channels
                .iter()
                .map(|(c, v)| {
                    (
                        c.to_string(),
                        crate::jsonstructs_versionsdb::JuliaupVersionDBChannel {
                            version: v.to_string(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn merge_first_server_wins_and_absolutizes() {
        let primary = db(
            "1.0.91",
            &[(
                "1.12.6+0.x64.linux.gnu",
                "bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz",
            )],
            &[("release", "1.12.6+0.x64.linux.gnu")],
        );
        let hub = Url::parse("https://internal.juliahub.com/juliabin/").unwrap();
        let hub_db = db(
            "1.0.130",
            &[
                (
                    "1.12.6+0.x64.linux.gnu",
                    "bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz",
                ),
                (
                    "1.12.7+dyad-3x3x0.x64.linux.gnu",
                    "dyadbin/dyad-linux-x86_64--refs-tags-v3.3.0.tar.gz",
                ),
            ],
            &[
                ("release", "1.12.7+dyad-3x3x0.x64.linux.gnu"),
                ("dyad-3.3.0", "1.12.7+dyad-3x3x0.x64.linux.gnu"),
            ],
        );

        let merged = merge_version_dbs(vec![(None, primary), (Some(&hub), hub_db)]);

        assert_eq!(merged.version, "1.0.91");
        assert_eq!(
            merged.available_channels["release"].version, "1.12.6+0.x64.linux.gnu",
            "the primary's channel wins"
        );
        assert_eq!(
            merged.available_channels["dyad-3.3.0"].version,
            "1.12.7+dyad-3x3x0.x64.linux.gnu"
        );
        assert_eq!(
            merged.available_versions["1.12.6+0.x64.linux.gnu"].url_path,
            "bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz",
            "the primary's entry stays relative"
        );
        assert_eq!(
            merged.available_versions["1.12.7+dyad-3x3x0.x64.linux.gnu"].url_path,
            "https://internal.juliahub.com/juliabin/dyadbin/dyad-linux-x86_64--refs-tags-v3.3.0.tar.gz"
        );

        // What the download step does with an absolute UrlPath.
        let base = Url::parse("https://julialang-s3.julialang.org/").unwrap();
        assert_eq!(
            base.join(&merged.available_versions["1.12.7+dyad-3x3x0.x64.linux.gnu"].url_path)
                .unwrap()
                .as_str(),
            "https://internal.juliahub.com/juliabin/dyadbin/dyad-linux-x86_64--refs-tags-v3.3.0.tar.gz"
        );
    }

    #[test]
    fn merge_server_before_primary_shadows_it() {
        let primary = db("1.0.91", &[], &[("release", "1.12.6+0.x64.linux.gnu")]);
        let mirror = Url::parse("https://mirror.example/").unwrap();
        let mirror_db = db("1.0.1", &[], &[("release", "1.12.5+0.x64.linux.gnu")]);

        let merged = merge_version_dbs(vec![(Some(&mirror), mirror_db), (None, primary)]);

        assert_eq!(
            merged.version, "1.0.91",
            "the version is always the primary's"
        );
        assert_eq!(
            merged.available_channels["release"].version,
            "1.12.5+0.x64.linux.gnu"
        );
    }

    #[test]
    fn cache_id_is_filesystem_safe_and_distinct() {
        let id = |s: &str| server_cache_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://internal.juliahub.com/juliabin/"),
            "internal.juliahub.com_juliabin"
        );
        assert_eq!(id("http://localhost:8899/"), "localhost_8899");
        assert_ne!(
            id("https://internal.juliahub.com/juliabin/"),
            id("https://internal.juliahub.com/other/")
        );
        assert_ne!(id("https://h.example/a_b/"), id("https://h.example/a/b/"));
        assert!(id("https://h.example/a%20b/c:d")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')));
    }

    #[test]
    fn server_url_from_arg_defaults_to_https() {
        assert_eq!(
            server_url_from_arg("julia.example.com").unwrap().as_str(),
            "https://julia.example.com/"
        );
        assert_eq!(
            server_url_from_arg(" https://julia.example.com/dist ")
                .unwrap()
                .as_str(),
            "https://julia.example.com/dist/"
        );
        assert!(server_url_from_arg("http://julia.example.com").is_err());
        assert!(server_url_from_arg("http://localhost:8899").is_ok());
    }

    #[test]
    fn effective_order_is_first_primary_rest() {
        let config = JuliaupConfig {
            servers: vec![
                JuliaupConfigServer {
                    url: "https://after.example/".into(),
                    name: Some("after".into()),
                    before_primary: false,
                },
                JuliaupConfigServer {
                    url: "https://before.example/".into(),
                    name: Some("before".into()),
                    before_primary: true,
                },
                JuliaupConfigServer {
                    url: "ftp://bad.example/".into(),
                    name: Some("bad".into()),
                    before_primary: false,
                },
            ],
            ..Default::default()
        };
        let names: Vec<String> = effective_servers(&config)
            .unwrap()
            .iter()
            .map(|s| s.display_name())
            .collect();
        assert_eq!(names, vec!["before", "primary", "after"]);
    }

    #[test]
    fn find_server_by_name_or_url() {
        let config = JuliaupConfig {
            servers: vec![JuliaupConfigServer {
                url: "https://internal.juliahub.com/juliabin/".into(),
                name: Some("juliahub".into()),
                before_primary: false,
            }],
            ..Default::default()
        };
        assert_eq!(find_server(&config, "juliahub"), Some(0));
        assert_eq!(
            find_server(&config, "https://internal.juliahub.com/juliabin"),
            Some(0)
        );
        assert_eq!(
            find_server(&config, "https://internal.juliahub.com/juliabin/"),
            Some(0)
        );
        assert_eq!(
            find_server(&config, "internal.juliahub.com/juliabin"),
            Some(0)
        );
        assert_eq!(find_server(&config, "other"), None);
    }
}
