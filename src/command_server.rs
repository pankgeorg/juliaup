use crate::config_file::{
    load_config_db, load_mut_config_db, save_config_db, JuliaupConfig, JuliaupConfigFile,
    JuliaupConfigServer,
};
use crate::global_paths::GlobalPaths;
use crate::servers::{
    effective_servers, find_server, load_server_versiondb, merged_versions_db,
    refresh_server_versiondb, server_cache_id, server_url_from_arg, server_versiondb_path, Refresh,
    SERVER_DBVERSION_PATH,
};
use crate::utils::{
    get_juliaserver_base_url, parse_server_url, print_juliaup_style, JuliaupMessageType,
};
use crate::versions_file::load_primary_versions_db;
use anyhow::{bail, Context, Result};
use cli_table::{
    format::{Border, HorizontalLine, Separator},
    print_stdout, ColorChoice, Table, WithTitle,
};
use url::Url;

#[derive(Table)]
struct ServerRow {
    #[table(title = "Order")]
    order: usize,
    #[table(title = "Name")]
    name: String,
    #[table(title = "URL")]
    url: String,
    #[table(title = "Database")]
    database: String,
    #[table(title = "Channels")]
    channels: String,
}

pub fn run_command_server_list(paths: &GlobalPaths) -> Result<()> {
    let config_file = load_config_db(paths, None)
        .with_context(|| "`server list` command failed to load configuration data.")?;
    let servers = effective_servers(&config_file.data)?;

    let mut rows = Vec::with_capacity(servers.len());
    for (i, server) in servers.iter().enumerate() {
        let (database, channels) = if server.primary {
            let db = load_primary_versions_db(paths)?;
            (db.version, db.available_channels.len().to_string())
        } else {
            match load_server_versiondb(&server_versiondb_path(paths, &server.url)) {
                Some(db) => (db.version, db.available_channels.len().to_string()),
                None => ("(not cached)".to_string(), "-".to_string()),
            }
        };
        rows.push(ServerRow {
            order: i + 1,
            name: server.display_name(),
            url: server.url.to_string(),
            database,
            channels,
        });
    }

    print_stdout(
        rows.with_title()
            .color_choice(ColorChoice::Never)
            .border(Border::builder().build())
            .separator(
                Separator::builder()
                    .title(Some(HorizontalLine::new('1', '2', '3', '-')))
                    .build(),
            ),
    )?;
    Ok(())
}

/// Fails when `name` is already used by a server other than the one at `url`.
fn check_name_free(config: &JuliaupConfig, url: &Url, name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        if let Some(index) = find_server(config, name) {
            if parse_server_url(&config.servers[index].url).ok().as_ref() != Some(url) {
                bail!("A server named `{name}` is already configured.");
            }
        }
    }
    Ok(())
}

pub fn run_command_server_add(
    url: &str,
    name: Option<String>,
    first: bool,
    paths: &GlobalPaths,
) -> Result<()> {
    let url = server_url_from_arg(url)?;
    let primary = get_juliaserver_base_url()?;
    if url == primary {
        bail!("`{url}` is already the primary server; set JULIAUP_SERVER to change that.");
    }
    if let Some(name) = &name {
        if name.is_empty() || name == "primary" || name.contains(char::is_whitespace) {
            bail!("`{name}` is not usable as a server name.");
        }
    }

    // Name conflicts fail before any network traffic. Whether the URL is
    // already configured decides how a failed fetch is reported.
    let configured = {
        let config_file = load_config_db(paths, None)
            .with_context(|| "`server add` command failed to load configuration data.")?;
        check_name_free(&config_file.data, &url, name.as_deref())?;
        find_server(&config_file.data, url.as_str()).is_some()
    };

    // Fetch the server's database with no lock held. This is where an
    // unreachable or non-juliaup server is rejected.
    print_juliaup_style(
        "Checking",
        &format!("the version database at {url}"),
        JuliaupMessageType::Progress,
    );
    let refresh = match refresh_server_versiondb(paths, &url, SERVER_DBVERSION_PATH) {
        Ok(refresh) => refresh,
        Err(err) if configured => {
            return Err(err.context(format!(
                "`{url}` is configured, but its version database could not be read."
            )));
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(paths.serversdir.join(server_cache_id(&url)));
            return Err(err.context(format!(
                "`{url}` does not serve a juliaup version database."
            )));
        }
    };
    let database = match &refresh {
        Refresh::Downloaded(version) => format!("database updated to {version}"),
        Refresh::UpToDate(version) => format!("database {version} up to date"),
    };

    let mut config_file = load_mut_config_db(paths)
        .with_context(|| "`server add` command failed to load configuration data.")?;
    check_name_free(&config_file.data, &url, name.as_deref())?;

    if let Some(index) = find_server(&config_file.data, url.as_str()) {
        return update_configured_server(&mut config_file, index, name, first, &database, paths);
    }

    let primary_db = load_primary_versions_db(paths)?;
    let before = merged_versions_db(paths, &config_file.data, primary_db.clone())?;
    config_file
        .data
        .servers
        .push(JuliaupConfigServer::new(&url, name.clone(), first));
    let after = merged_versions_db(paths, &config_file.data, primary_db)?;

    save_config_db(&mut config_file, paths)
        .with_context(|| "Failed to save the configuration file.")?;

    let own = load_server_versiondb(&server_versiondb_path(paths, &url))
        .map(|db| db.available_channels.len())
        .unwrap_or(0);
    let gained = after
        .available_channels
        .keys()
        .filter(|c| !before.available_channels.contains_key(*c))
        .count();
    let changed = after
        .available_channels
        .iter()
        .filter(|(c, v)| {
            before
                .available_channels
                .get(*c)
                .is_some_and(|b| b.version != v.version)
        })
        .count();

    let label = name.unwrap_or_else(|| url.to_string());
    let mut summary = format!("server {label} ({database}): {gained} new channel(s)");
    if first {
        summary.push_str(&format!(", {changed} existing channel(s) now resolve here"));
    } else {
        summary.push_str(&format!(
            ", {} already defined by an earlier server",
            own.saturating_sub(gained)
        ));
    }
    print_juliaup_style("Added", &summary, JuliaupMessageType::Success);
    Ok(())
}

/// Applies `--name` and `--first` to a server that is already configured, so
/// that repeating `server add` converges on the requested state instead of
/// failing.
fn update_configured_server(
    config_file: &mut JuliaupConfigFile,
    index: usize,
    name: Option<String>,
    first: bool,
    database: &str,
    paths: &GlobalPaths,
) -> Result<()> {
    let server = &mut config_file.data.servers[index];
    let mut changes = Vec::new();
    if let Some(name) = name {
        if server.name.as_ref() != Some(&name) {
            changes.push(format!("now named {name}"));
            server.name = Some(name);
        }
    }
    if server.before_primary != first {
        changes.push(if first {
            "now consulted before the primary server".to_string()
        } else {
            "now consulted after the primary server".to_string()
        });
        server.before_primary = first;
    }
    let label = server.name.clone().unwrap_or_else(|| server.url.clone());

    if changes.is_empty() {
        print_juliaup_style(
            "Unchanged",
            &format!("server {label} is already configured ({database})."),
            JuliaupMessageType::Success,
        );
        return Ok(());
    }

    save_config_db(config_file, paths).with_context(|| "Failed to save the configuration file.")?;
    print_juliaup_style(
        "Updated",
        &format!("server {label}: {} ({database}).", changes.join(", ")),
        JuliaupMessageType::Success,
    );
    Ok(())
}

pub fn run_command_server_remove(server: &str, paths: &GlobalPaths) -> Result<()> {
    let mut config_file = load_mut_config_db(paths)
        .with_context(|| "`server remove` command failed to load configuration data.")?;

    let Some(index) = find_server(&config_file.data, server) else {
        if server == "primary"
            || server_url_from_arg(server).ok() == get_juliaserver_base_url().ok()
        {
            bail!("The primary server cannot be removed; set JULIAUP_SERVER to replace it.");
        }
        bail!("No server named or at `{server}` is configured; see `juliaup server list`.");
    };

    let removed = config_file.data.servers.remove(index);
    save_config_db(&mut config_file, paths)
        .with_context(|| "Failed to save the configuration file.")?;

    if let Ok(url) = parse_server_url(&removed.url) {
        let _ = std::fs::remove_dir_all(paths.serversdir.join(server_cache_id(&url)));
    }

    print_juliaup_style(
        "Removed",
        &format!(
            "server {}. Channels installed from it stay installed.",
            removed.name.unwrap_or(removed.url)
        ),
        JuliaupMessageType::Success,
    );
    Ok(())
}
