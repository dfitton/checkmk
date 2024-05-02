// Copyright (C) 2023 Checkmk GmbH - License: GNU General Public License v2
// This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
// conditions defined in the file COPYING, which is part of this source code package.

use super::client::{self, Client};
use super::custom::get_sql_dir;
use super::section::{Section, SectionKind};
use crate::config::section;
use crate::config::{
    self,
    ms_sql::{AuthType, CustomInstance, Endpoint},
    section::names,
    CheckConfig,
};
use crate::emit;
use crate::ms_sql::query::{
    obtain_computer_name, obtain_instance_name, run_custom_query, run_known_query, Answer, Column,
};
use crate::ms_sql::sqls;
use crate::setup::Env;
use crate::types::{
    ComputerName, HostName, InstanceAlias, InstanceCluster, InstanceEdition, InstanceId,
    InstanceName, InstanceVersion, PiggybackHostName, Port,
};
use crate::utils;
use core::fmt;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use tiberius::Row;

pub const SQL_LOGIN_ERROR_TAG: &str = "[SQL LOGIN ERROR]";
pub const SQL_TCP_ERROR_TAG: &str = "[SQL TCP ERROR]";

#[derive(Clone, Debug, Default)]
pub struct SqlInstanceBuilder {
    alias: Option<InstanceAlias>,
    pub name: Option<InstanceName>,
    id: Option<InstanceId>,
    edition: Option<InstanceEdition>,
    version: Option<InstanceVersion>,
    cluster: Option<InstanceCluster>,
    port: Option<Port>,
    dynamic_port: Option<Port>,
    endpoint: Option<Endpoint>,
    computer_name: Option<ComputerName>,
    environment: Option<Env>,
    cache_dir: Option<String>,
    piggyback: Option<PiggybackHostName>,
}

impl SqlInstanceBuilder {
    pub fn new() -> SqlInstanceBuilder {
        SqlInstanceBuilder::default()
    }

    pub fn name<S: Into<String>>(mut self, name: S) -> Self {
        self.name = Some(name.into().to_uppercase().into());
        self
    }
    pub fn alias(mut self, alias: &Option<InstanceAlias>) -> Self {
        self.alias = alias.clone();
        self
    }
    pub fn id<S: Into<String>>(mut self, id: S) -> Self {
        self.id = Some(InstanceId::from(id.into()));
        self
    }
    pub fn edition(mut self, edition: &InstanceEdition) -> Self {
        self.edition = Some(edition.clone());
        self
    }
    pub fn version(mut self, version: &InstanceVersion) -> Self {
        self.version = Some(version.clone());
        self
    }
    pub fn cluster(mut self, cluster: Option<InstanceCluster>) -> Self {
        self.cluster = cluster;
        self
    }
    pub fn port(mut self, port: Option<Port>) -> Self {
        self.port = port;
        self
    }
    pub fn dynamic_port(mut self, port: Option<Port>) -> Self {
        self.dynamic_port = port;
        self
    }
    pub fn endpoint(mut self, endpoint: &Endpoint) -> Self {
        self.endpoint = Some(endpoint.clone());
        self
    }
    pub fn computer_name(mut self, computer_name: Option<ComputerName>) -> Self {
        self.computer_name = computer_name;
        self
    }

    pub fn environment(mut self, environment: &Env) -> Self {
        self.environment = environment.clone().into();
        self
    }
    pub fn cache_dir(mut self, cache_dir: &str) -> Self {
        self.cache_dir = Some(cache_dir.to_owned());
        self
    }
    pub fn piggyback(mut self, piggyback: Option<PiggybackHostName>) -> Self {
        self.piggyback = piggyback.map(|s| s.to_string().to_lowercase().into());
        self
    }

    pub fn row(self, row: &Row) -> Self {
        self.name(row.get_value_by_idx(0))
            .id(row.get_value_by_idx(1))
            .edition(&row.get_value_by_idx(2).into())
            .version(&row.get_value_by_idx(3).into())
            .cluster(row.get_optional_value_by_idx(4).map(|s| s.into()))
            .port(
                row.get_optional_value_by_idx(5)
                    .and_then(|s| s.parse::<u16>().ok())
                    .map(Port),
            )
            .dynamic_port(
                row.get_optional_value_by_idx(6)
                    .and_then(|s| s.parse::<u16>().ok())
                    .map(Port),
            )
    }

    pub fn get_name(&self) -> InstanceName {
        self.name.clone().unwrap_or_default()
    }

    pub fn get_endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    pub fn get_port(&self) -> Port {
        self.get_port_ref().cloned().unwrap_or(Port(0))
    }

    fn get_port_ref(&self) -> Option<&Port> {
        self.port.as_ref().or(self.dynamic_port.as_ref())
    }

    pub fn build(self) -> SqlInstance {
        let version_table = parse_version(&self.version);
        SqlInstance {
            alias: self.alias,
            name: self.name.unwrap_or_default(),
            id: self.id.unwrap_or_default(),
            edition: self.edition.unwrap_or_default(),
            version: self.version.unwrap_or_default(),
            cluster: self.cluster,
            port: self.port,
            dynamic_port: self.dynamic_port,
            available: None,
            endpoint: self.endpoint.unwrap_or_default(),
            computer_name: self.computer_name,
            environment: self.environment.unwrap_or_default(),
            cache_dir: self.cache_dir.unwrap_or_default(),
            piggyback: self.piggyback,
            version_table,
        }
    }
}

fn parse_version(version: &Option<InstanceVersion>) -> [u32; 3] {
    if let Some(version) = version {
        let version = version.to_string();
        let mut parts = version.split('.');
        let major = parts.next().and_then(|s| s.parse::<u32>().ok());
        let minor = parts.next().and_then(|s| s.parse::<u32>().ok());
        let build = parts.next().and_then(|s| s.parse::<u32>().ok());
        [major.unwrap_or(0), minor.unwrap_or(0), build.unwrap_or(0)]
    } else {
        [0, 0, 0]
    }
}

#[derive(Clone, Debug)]
pub struct SqlInstance {
    pub alias: Option<InstanceAlias>,
    pub name: InstanceName,
    pub id: InstanceId,
    pub version: InstanceVersion,
    pub edition: InstanceEdition,
    pub cluster: Option<InstanceCluster>,
    port: Option<Port>,
    dynamic_port: Option<Port>,
    pub available: Option<bool>,
    endpoint: Endpoint,
    computer_name: Option<ComputerName>,
    environment: Env,
    cache_dir: String,
    piggyback: Option<PiggybackHostName>,
    version_table: [u32; 3],
}

impl AsRef<SqlInstance> for SqlInstance {
    fn as_ref(&self) -> &SqlInstance {
        self
    }
}

impl fmt::Display for SqlInstance {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} `{}` `{}` [{}:{}]",
            self.full_name(),
            self.version,
            self.edition,
            self.port
                .clone()
                .map(|p| u16::from(p).to_string())
                .unwrap_or("None".to_string()),
            self.dynamic_port
                .clone()
                .map(|p| u16::from(p).to_string())
                .unwrap_or("None".to_string())
        )
    }
}

impl SqlInstance {
    pub fn generate_leading_entry(&self, sep: char) -> String {
        format!(
            "{}{sep}config{sep}{}{sep}{}{sep}{}\n",
            self.mssql_name(),
            self.version,
            self.edition,
            self.cluster.clone().unwrap_or_default()
        )
    }

    pub fn mssql_name(&self) -> String {
        format!("MSSQL_{}", self.name)
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.endpoint.hostname(), self.name)
    }

    pub fn cache_dir(&self) -> &str {
        &self.cache_dir
    }

    pub fn temp_dir(&self) -> Option<&Path> {
        self.environment.temp_dir()
    }

    pub fn piggyback(&self) -> &Option<PiggybackHostName> {
        &self.piggyback
    }

    pub fn hostname(&self) -> HostName {
        self.endpoint.hostname()
    }

    /// not tested, because it is a bit legacy
    pub fn legacy_name(&self) -> String {
        if self.name.to_string() != "MSSQLSERVER" {
            return format!("{}/{}", self.legacy_name_prefix(), self.name);
        }

        if let Some(cluster) = &self.cluster {
            cluster.clone().into()
        } else {
            "(local)".to_string()
        }
    }

    fn legacy_name_prefix(&self) -> &str {
        if let Some(cluster) = &self.cluster {
            return cluster.into();
        }
        if let Some(computer_name) = &self.computer_name {
            computer_name.into()
        } else {
            ""
        }
    }

    pub fn version_major(&self) -> u32 {
        self.version_table[0]
    }

    pub fn version_minor(&self) -> u32 {
        self.version_table[1]
    }

    pub fn version_build(&self) -> u32 {
        self.version_table[2]
    }

    pub fn generate_header(&self) -> String {
        self.piggyback
            .as_ref()
            .map(emit::piggyback_header)
            .unwrap_or_default()
            .to_owned()
    }

    pub fn generate_footer(&self) -> String {
        self.piggyback
            .as_ref()
            .map(|_| emit::piggyback_footer())
            .unwrap_or_default()
            .to_owned()
    }

    pub async fn generate_sections(
        &self,
        ms_sql: &config::ms_sql::Config,
        sections: &[Section],
    ) -> String {
        let header = self.generate_header();
        let endpoint = &ms_sql.endpoint();

        // if yes - call generate_section with database parameter
        // else - call generate_section without database parameter
        log::trace!("{:?} @ {:?}", self, endpoint);
        let body = match self.create_client(endpoint, None).await {
            Ok(mut client) => {
                self._generate_sections(&mut client, endpoint, sections)
                    .await
            }
            Err(err) => {
                log::warn!("Can't access {} instance with err {err}\n", self.id);
                let instance_section = Section::make_instance_section(); // this is important section always present
                instance_section.to_plain_header()
                    + &self
                        .generate_bad_state_entry(instance_section.sep(), format!("{err}").as_str())
            }
        };
        header + &body + &self.generate_footer()
    }

    /// Gather databases based on sections content: only if any of sections is database based
    async fn gather_databases(&self, client: &mut Client, sections: &[Section]) -> Vec<String> {
        let database_based_sections = section::get_per_database_sections();
        let need = database_based_sections.iter().any(|s| {
            sections
                .iter()
                .map(|s| s.name().to_string())
                .collect::<Vec<String>>()
                .contains(s)
        });
        if need {
            self.generate_databases(client).await
        } else {
            Vec::new()
        }
    }

    pub async fn _generate_sections(
        &self,
        client: &mut Client,
        endpoint: &Endpoint,
        sections: &[Section],
    ) -> String {
        let mut data: Vec<String> = Vec::new();
        let databases = self.gather_databases(client, sections).await;
        for section in sections.iter() {
            data.push(
                self.generate_section(client, endpoint, section, &databases)
                    .await,
            );
        }
        data.join("")
    }

    /// Create a client for an Instance based on Config
    pub async fn create_client(
        &self,
        endpoint: &Endpoint,
        database: Option<String>,
    ) -> Result<Client> {
        log::info!("create_client {}", self.name);
        let (auth, conn) = endpoint.split();
        let client = match auth.auth_type() {
            AuthType::SqlServer | AuthType::Windows => {
                if let Some(credentials) = client::obtain_config_credentials(auth) {
                    client::ClientBuilder::new()
                        .logon_on_port(conn.hostname(), self.port(), credentials)
                        .database(database)
                } else {
                    anyhow::bail!("Not provided credentials")
                }
            }

            #[cfg(windows)]
            AuthType::Integrated => client::ClientBuilder::new()
                .local_by_port(self.port())
                .database(database),

            _ => anyhow::bail!("Not supported authorization type"),
        };
        client.build().await
    }

    pub async fn generate_details_entry(&self, client: &mut Client, sep: char) -> String {
        let r = SqlInstanceProperties::obtain_by_query(client).await;
        match r {
            Ok(properties) => self.process_details_rows(&properties, sep),
            Err(err) => {
                log::error!("Failed to get sql instance properties: {}", err);
                format!("{}{:?}", sep.to_string().repeat(4), err).to_string()
            }
        }
    }

    pub fn generate_good_state_entry(&self, sep: char) -> String {
        format!("{}{sep}state{sep}1{sep}\n", self.mssql_name(),)
    }

    pub fn generate_bad_state_entry(&self, sep: char, message: &str) -> String {
        format!("{}{sep}state{sep}0{sep}{}\n", self.mssql_name(), message)
    }

    pub async fn generate_section(
        &self,
        client: &mut Client,
        endpoint: &Endpoint,
        section: &Section,
        databases: &[String],
    ) -> String {
        let body = match self.read_data_from_cache(section.name(), section.cache_age() as u64) {
            Some(from_cache) => from_cache,
            None => {
                let from_sql = self
                    .generate_section_body(client, endpoint, section, databases)
                    .await;
                if section.kind() == &SectionKind::Async {
                    self.write_data_in_cache(section.name(), &from_sql);
                };
                from_sql
            }
        };
        section.to_work_header() + body.as_str()
    }

    async fn generate_section_body(
        &self,
        client: &mut Client,
        endpoint: &Endpoint,
        section: &Section,
        databases: &[String],
    ) -> String {
        if let Some(query) = section.select_query(get_sql_dir(), self.version_major()) {
            let sep = section.sep();
            match section.name() {
                names::INSTANCE => {
                    self.generate_good_state_entry(sep)
                        + &self.generate_details_entry(client, sep).await
                }
                names::COUNTERS => self.generate_counters_section(client, &query, sep).await,
                names::BACKUP => self.generate_backup_section(client, &query, sep).await,
                names::BLOCKED_SESSIONS => {
                    self.generate_sessions_section(client, &query, sep).await
                }
                names::DATABASES => {
                    self.generate_databases_section(client, databases, &query, sep)
                        .await
                }
                names::CONNECTIONS => self.generate_connections_section(client, &query, sep).await,
                names::TRANSACTION_LOG
                | names::TABLE_SPACES
                | names::DATAFILES
                | names::CLUSTERS => {
                    self.generate_database_indexed_section(
                        databases, endpoint, section, &query, sep,
                    )
                    .await
                }
                names::MIRRORING | names::JOBS | names::AVAILABILITY_GROUPS => {
                    self.generate_unified_section(endpoint, section, None).await
                }
                _ => self
                    .generate_custom_section(endpoint, section)
                    .await
                    .unwrap_or_else(|| {
                        format!(
                            "Can't find sql in for custom section `{}`\n",
                            section.name()
                        )
                        .to_string()
                    }),
            }
        } else {
            log::error!("Bad section query: {}", section.name());
            String::default()
        }
    }

    fn read_data_from_cache(&self, name: &str, cache_age: u64) -> Option<String> {
        if cache_age == 0 {
            return None;
        }
        if let Some(path) = self
            .environment
            .obtain_cache_sub_dir(self.cache_dir())
            .map(|d| d.join(self.make_cache_entry_name(name)))
        {
            match utils::get_modified_age(&path) {
                Ok(file_age) if file_age <= cache_age => {
                    log::info!("Cache file {path:?} is new enough for {cache_age} cache_age",);
                    std::fs::read_to_string(&path)
                        .map_err(|e| {
                            log::error!("{e} reading cache file {:?}", &path);
                            e
                        })
                        .ok()
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn write_data_in_cache(&self, name: &str, body: &str) {
        if let Some(dir) = self.environment.obtain_cache_sub_dir(self.cache_dir()) {
            let file_name = self.make_cache_entry_name(name);
            std::fs::write(dir.join(file_name), body)
                .unwrap_or_else(|e| log::error!("Error {e} writing cache"));
        }
    }

    fn make_cache_entry_name(&self, name: &str) -> String {
        format!("{};{};{}.mssql", self.hostname(), self.name, name)
    }

    pub async fn generate_counters_section(
        &self,
        client: &mut Client,
        query: &str,
        sep: char,
    ) -> String {
        let x = run_custom_query(client, query)
            .await
            .and_then(validate_rows_has_two_blocks)
            .and_then(|rows| {
                Ok(self.process_utc_rows(&rows[0], sep)?
                    + &self.process_counters_rows(&rows[1], sep)?)
            });
        match x {
            Ok(result) => result,
            Err(err) => {
                log::error!("Failed to get counters: {}", err);
                format!("{sep}{sep}{}{sep}{}\n", self.name, err).to_string()
            }
        }
    }

    pub async fn generate_counters_entry(&self, client: &mut Client, sep: char) -> String {
        let x = run_known_query(client, sqls::Id::CounterEntries)
            .await
            .and_then(validate_rows)
            .and_then(|rows| self.process_counters_rows(&rows[0], sep));
        match x {
            Ok(result) => result,
            Err(err) => {
                log::error!("Failed to get counters: {}", err);
                format!("{sep}{sep}{}{sep}{}\n", self.name, err).to_string()
            }
        }
    }

    fn process_counters_rows(&self, rows: &[Row], sep: char) -> Result<String> {
        let z: Vec<String> = rows.iter().map(|row| to_counter_entry(row, sep)).collect();
        Ok(z.join(""))
    }

    pub async fn generate_sessions_section(
        &self,
        client: &mut Client,
        query: &str,
        sep: char,
    ) -> String {
        match run_custom_query(client, query).await {
            Ok(rows) => {
                if rows.is_empty() || rows[0].is_empty() {
                    log::info!("No blocking sessions");
                    return format!("{}{sep}No blocking sessions\n", self.name).to_string();
                }
                self.process_blocked_sessions_rows(&rows, sep)
            }
            Err(err) => {
                log::info!("No blocking sessions: {}", err);
                format!("{}{sep}{err:?}\n", self.name).to_string()
            }
        }
    }

    pub async fn generate_table_spaces_section(
        &self,
        endpoint: &Endpoint,
        databases: &[String],
        query: &str,
        sep: char,
    ) -> String {
        let format_error = |d: &str, e: &anyhow::Error| {
            format!(
                "{} {} - - - - - - - - - - - - {:?}\n",
                self.mssql_name(),
                d.replace(' ', "_"),
                e
            )
            .to_string()
        };
        let mut result = String::new();
        for d in databases {
            match self.create_client(endpoint, Some(d.clone())).await {
                Ok(mut c) => {
                    result += &run_custom_query(&mut c, query)
                        .await
                        .map(|rows| to_table_spaces_entry(&self.mssql_name(), d, &rows, sep))
                        .unwrap_or_else(|e| format_error(d, &e));
                }
                Err(err) => {
                    result += &format_error(d, &err);
                }
            }
        }
        result
    }

    pub async fn generate_backup_section(
        &self,
        client: &mut Client,
        query: &str,
        sep: char,
    ) -> String {
        let databases = self.generate_databases(client).await;

        let result = run_custom_query(client, query)
            .await
            .map(|rows| self.process_backup_rows(&rows, &databases, sep));
        match result {
            Ok(output) => output,
            Err(err) => {
                log::error!("Failed to get backup: {}", err);
                databases
                    .iter()
                    .map(|d| {
                        format!(
                            "{}{sep}{}{sep}-{sep}-{sep}-{sep}{:?}\n",
                            self.mssql_name(),
                            d.replace(' ', "_"),
                            err
                        )
                    })
                    .collect::<Vec<String>>()
                    .join("")
            }
        }
    }

    pub async fn generate_database_indexed_section(
        &self,
        databases: &[String],
        endpoint: &Endpoint,
        section: &Section,
        query: &str,
        sep: char,
    ) -> String {
        match section.name() {
            names::TRANSACTION_LOG => {
                self.generate_transaction_logs_section(endpoint, databases, query, sep)
                    .await
            }
            names::TABLE_SPACES => {
                self.generate_table_spaces_section(endpoint, databases, query, sep)
                    .await
            }
            names::DATAFILES => {
                self.generate_datafiles_section(endpoint, databases, query, sep)
                    .await
            }
            names::CLUSTERS => {
                self.generate_clusters_section(endpoint, databases, query, sep)
                    .await
            }
            _ => format!("{} not implemented\n", section.name()).to_string(),
        }
    }

    pub async fn generate_transaction_logs_section(
        &self,
        endpoint: &Endpoint,
        databases: &[String],
        query: &str,
        sep: char,
    ) -> String {
        let mut result = String::new();
        for d in databases {
            match self.create_client(endpoint, Some(d.clone())).await {
                Ok(mut c) => {
                    result += &run_custom_query(&mut c, query)
                        .await
                        .map(|rows| to_transaction_logs_entries(&self.name, d, &rows, sep))
                        .unwrap_or_else(|e| self.format_some_file_error(d, &e, sep));
                }
                Err(err) => {
                    result += &self.format_some_file_error(d, &err, sep);
                }
            }
        }
        result
    }

    fn format_some_file_error(&self, d: &str, e: &anyhow::Error, sep: char) -> String {
        format!(
            "{}{sep}{}|-|-|-|-|-|-|{:?}\n",
            self.name,
            d.replace(' ', "_"),
            e
        )
        .to_string()
    }

    pub async fn generate_datafiles_section(
        &self,
        endpoint: &Endpoint,
        databases: &[String],
        query: &str,
        sep: char,
    ) -> String {
        let mut result = String::new();
        for d in databases {
            match self.create_client(endpoint, Some(d.clone())).await {
                Ok(mut c) => {
                    result += &run_custom_query(&mut c, query)
                        .await
                        .map(|rows| to_datafiles_entries(&self.name, d, &rows, sep))
                        .unwrap_or_else(|e| self.format_some_file_error(d, &e, sep));
                }
                Err(err) => {
                    result += &self.format_some_file_error(d, &err, sep);
                }
            }
        }
        result
    }

    pub async fn generate_databases_section(
        &self,
        client: &mut Client,
        databases: &[String],
        query: &str,
        sep: char,
    ) -> String {
        run_custom_query(client, query)
            .await
            .map(|rows| to_databases_entries(&self.name, &rows, sep))
            .unwrap_or_else(|e| {
                databases
                    .iter()
                    .map(|d| self.format_databases_error(d, &e, sep))
                    .collect::<Vec<String>>()
                    .join("")
            })
    }

    fn format_databases_error(&self, d: &str, e: &anyhow::Error, sep: char) -> String {
        format!(
            "{}{sep}{}{sep}{}{}\n",
            self.name,
            d.replace(' ', "_"),
            e,
            format!("{sep}-").repeat(3),
        )
    }

    /// doesn't return error - the same behavior as plugin
    pub async fn generate_databases(&self, client: &mut Client) -> Vec<String> {
        let result = run_known_query(client, sqls::Id::DatabaseNames)
            .await
            .and_then(validate_rows)
            .map(|rows| self.process_databases_rows(&rows));
        match result {
            Ok(result) => result,
            Err(err) => {
                log::error!("Failed to get databases: {}", err);
                vec![]
            }
        }
    }

    /// Todo(sk): write a test
    pub async fn generate_clusters_section(
        &self,
        endpoint: &Endpoint,
        databases: &[String],
        query: &str,
        sep: char,
    ) -> String {
        let format_error = |d: &str, e: &anyhow::Error| {
            format!(
                "{}{sep}{}{sep}{sep}{sep}{:?}\n",
                self.name,
                d.replace(' ', "_"),
                e
            )
        };
        let mut result = String::new();
        for database in databases {
            match self.create_client(endpoint, Some(database.clone())).await {
                Ok(mut c) => match self
                    .generate_clusters_entry(&mut c, database, query, sep)
                    .await
                {
                    Ok(None) => {}
                    Ok(Some(entry)) => result += &entry,
                    Err(err) => result += &format_error(database, &err),
                },
                Err(err) => {
                    result += &format_error(database, &err);
                }
            }
        }
        result
    }

    async fn generate_clusters_entry(
        &self,
        client: &mut Client,
        database: &str,
        query: &str,
        sep: char,
    ) -> Result<Option<String>> {
        if !self.is_database_clustered(client).await? {
            return Ok(None);
        }
        let (nodes, active_node) = self.get_cluster_nodes(client, query).await?;
        Ok(Some(format!(
            "{}{sep}{}{sep}{}{sep}{}",
            self.name,
            database.replace(' ', "_"),
            active_node,
            nodes
        )))
    }

    async fn is_database_clustered(&self, client: &mut Client) -> Result<bool> {
        let rows = &run_known_query(client, sqls::Id::IsClustered)
            .await
            .and_then(validate_rows)?;
        Ok(&rows[0][0].get_value_by_name("is_clustered") != "0")
    }

    async fn get_cluster_nodes(
        &self,
        client: &mut Client,
        query: &str,
    ) -> Result<(String, String)> {
        let rows = &run_custom_query(client, query).await?;
        if rows.len() > 2 && !rows[0].is_empty() && !rows[1].is_empty() {
            return Ok((
                rows[0]
                    .iter()
                    .map(|r| r.get_value_by_name("nodename"))
                    .collect::<Vec<String>>()
                    .join(","),
                rows[1]
                    .last() // as in legacy plugin
                    .expect("impossible")
                    .get_value_by_name("active_node"),
            ));
        }
        Ok((String::default(), String::default()))
    }

    pub async fn generate_connections_section(
        &self,
        client: &mut Client,
        query: &str,
        sep: char,
    ) -> String {
        run_custom_query(client, query)
            .await
            .map(|rows| self.to_connections_entries(&rows, sep))
            .unwrap_or_else(|e| format!("{}{sep}{}\n", self.name, e))
    }

    fn to_connections_entries(&self, rows: &[Vec<Row>], sep: char) -> String {
        if rows.is_empty() {
            return String::new();
        }
        let rows = &rows[0];
        rows.iter()
            .map(|row| {
                format!(
                    "{}{sep}{}{sep}{}\n",
                    self.name,
                    row.get_value_by_idx(0).replace(' ', "_"), // for unknown reason we can't get it by name
                    row.get_bigint_by_name("NumberOfConnections")
                )
            })
            .collect::<Vec<String>>()
            .join("")
    }

    /// NOTE: uses ' ' instead of '\t' in error messages
    pub async fn generate_unified_section(
        &self,
        endpoint: &Endpoint,
        section: &Section,
        query: Option<&str>,
    ) -> String {
        match self.create_client(endpoint, section.main_db()).await {
            Ok(mut c) => {
                let q = query.map(|q| q.to_owned()).unwrap_or_else(|| {
                    section
                        .select_query(get_sql_dir(), self.version_major())
                        .unwrap_or_default()
                });
                run_custom_query(&mut c, q)
                    .await
                    .and_then(|r| section.validate_rows(r))
                    .map(|rows| {
                        format!(
                            "{}{}",
                            section.first_line(Some(&self.name)),
                            self.to_entries(rows, section.sep())
                        )
                    })
                    .unwrap_or_else(|e| format!("{} {}\n", self.name, e))
            }
            Err(err) => format!("{} {}\n", self.name, err),
        }
    }

    pub async fn generate_custom_section(
        &self,
        endpoint: &Endpoint,
        section: &Section,
    ) -> Option<String> {
        match self.create_client(endpoint, None).await {
            Ok(mut c) => {
                if let Some(query) =
                    section.find_provided_query(get_sql_dir(), self.version_major())
                {
                    Some(
                        run_custom_query(&mut c, query)
                            .await
                            .and_then(|r| section.validate_rows(r))
                            .map(|rows| {
                                format!(
                                    "{}{}",
                                    section.first_line(Some(&self.name)),
                                    self.to_entries(rows, section.sep())
                                )
                            })
                            .unwrap_or_else(|e| format!("{} {}\n", self.name, e)),
                    )
                } else {
                    None
                }
            }
            Err(err) => Some(format!("{} {}\n", self.name, err)),
        }
    }

    /// rows must be not empty
    fn to_entries(&self, rows: Vec<Vec<Row>>, sep: char) -> String {
        // just a safety guard, the function should not get empty rows
        if rows.is_empty() {
            return String::new();
        }

        let mut r = rows;
        let rows = r.remove(0);
        let result = rows
            .into_iter()
            .map(|r| r.get_all(sep))
            .collect::<Vec<String>>()
            .join("\n");

        if result.is_empty() {
            result
        } else {
            result + "\n"
        }
    }

    fn process_blocked_sessions_rows(&self, rows: &[Vec<Row>], sep: char) -> String {
        let rows = &rows[0];
        rows.iter()
            .map(|row| to_blocked_session_entry(&self.name, row, sep))
            .collect::<Vec<String>>()
            .join("")
    }

    fn process_utc_rows(&self, rows: &[Row], sep: char) -> Result<String> {
        let utc = rows[0].get_value_by_name(sqls::UTC_DATE_FIELD);
        Ok(format!("None{sep}utc_time{sep}None{sep}{utc}\n"))
    }

    fn process_databases_rows(&self, rows: &[Vec<Row>]) -> Vec<String> {
        let row = &rows[0];
        row.iter()
            .map(|row| row.get_value_by_idx(0))
            .collect::<Vec<String>>()
    }

    fn process_details_rows(&self, properties: &SqlInstanceProperties, sep: char) -> String {
        format!(
            "{}{sep}details{sep}{}{sep}{}{sep}{}\n",
            self.mssql_name(),
            properties.version,
            properties.product_level,
            properties.edition
        )
    }

    fn process_backup_rows(&self, rows: &Vec<Vec<Row>>, databases: &[String], sep: char) -> String {
        let (mut ready, missing_data) = self.process_backup_rows_partly(rows, databases, sep);
        let missing: Vec<String> = self.process_missing_backup_rows(&missing_data, sep);
        ready.extend(missing);
        ready.join("")
    }

    /// generates lit of correct backup entries + list of missing required backups
    fn process_backup_rows_partly(
        &self,
        rows: &Vec<Vec<Row>>,
        databases: &[String],
        sep: char,
    ) -> (Vec<String>, HashSet<String>) {
        let mut only_databases: HashSet<String> =
            databases.iter().map(|s| s.to_lowercase()).collect();
        let s: Vec<String> = if !rows.is_empty() {
            rows[0]
                .iter()
                .filter_map(|row| {
                    let backup_database = row.get_value_by_name("database_name").to_lowercase();
                    if only_databases.contains(&backup_database) {
                        only_databases.remove(&backup_database);
                        to_backup_entry(&self.mssql_name(), &backup_database, row, sep)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            vec![]
        };
        (s, only_databases)
    }

    fn process_missing_backup_rows(&self, data: &HashSet<String>, sep: char) -> Vec<String> {
        data.iter()
            .map(|db| {
                format!(
                    "{}{sep}{}{sep}-{sep}-{sep}-{sep}No backup found\n",
                    self.mssql_name(),
                    db.replace(' ', "_")
                )
            })
            .collect()
    }

    pub fn port(&self) -> Option<Port> {
        self.dynamic_port.clone().or(self.port.clone())
    }

    pub fn computer_name(&self) -> &Option<ComputerName> {
        &self.computer_name
    }
}

#[derive(Debug)]
pub struct SqlInstanceProperties {
    pub name: InstanceName,
    pub version: InstanceVersion,
    pub computer_name: ComputerName,
    pub edition: InstanceEdition,
    pub product_level: String,
    pub net_bios: String,
}

impl From<&Vec<Row>> for SqlInstanceProperties {
    fn from(row: &Vec<Row>) -> Self {
        let row = &row[0];
        let name = row.get_value_by_name("InstanceName");
        let version: InstanceVersion = row.get_value_by_name("ProductVersion").into();
        let computer_name: ComputerName = row.get_value_by_name("MachineName").into();
        let edition: InstanceEdition = row.get_value_by_name("Edition").into();
        let product_level = row.get_value_by_name("ProductLevel");
        let net_bios = row.get_value_by_name("NetBios");
        Self {
            name: (if name.is_empty() {
                "MSSQLSERVER".to_string()
            } else {
                name.to_uppercase()
            })
            .into(),
            version,
            computer_name,
            edition,
            product_level,
            net_bios,
        }
    }
}

impl SqlInstanceProperties {
    pub async fn obtain_by_query(client: &mut Client) -> Result<Self> {
        let r = run_known_query(client, sqls::Id::InstanceProperties).await?;
        if r.is_empty() {
            anyhow::bail!("Empty answer from server on query instance_properties");
        }
        Ok(Self::from(&r[0]))
    }
}

fn validate_rows(rows: Vec<Vec<Row>>) -> Result<Vec<Vec<Row>>> {
    if rows.is_empty() || rows[0].is_empty() {
        Err(anyhow::anyhow!("No output from query"))
    } else {
        Ok(rows)
    }
}

fn validate_rows_has_two_blocks(rows: Vec<Vec<Row>>) -> Result<Vec<Vec<Row>>> {
    if rows.len() != 2 || rows[0].is_empty() || rows[1].is_empty() {
        Err(anyhow::anyhow!("Output from query is invalid"))
    } else {
        Ok(rows)
    }
}

fn to_table_spaces_entry(
    instance_name: &str,
    database_name: &str,
    rows: &Vec<Vec<Row>>,
    sep: char,
) -> String {
    let extract = |rows: &Vec<Vec<Row>>, part: usize, name: &str| {
        if (rows.len() < part) || rows[part].is_empty() {
            String::new()
        } else {
            rows[part][0].get_value_by_name(name).trim().to_string()
        }
    };
    let db_size = extract(rows, 0, "database_size");
    let unallocated = extract(rows, 0, "unallocated_space");
    let reserved = extract(rows, 1, "reserved");
    let data = extract(rows, 1, "data");
    let index_size = extract(rows, 1, "index_size");
    let unused = extract(rows, 1, "unused");
    format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}\n",
        instance_name,
        database_name.replace(' ', "_"),
        db_size,
        unallocated,
        reserved,
        data,
        index_size,
        unused
    )
}

fn to_transaction_logs_entries(
    instance_name: &InstanceName,
    database_name: &str,
    rows: &[Vec<Row>],
    sep: char,
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    rows[0]
        .iter()
        .map(|row| to_transaction_logs_entry(row, instance_name, database_name, sep))
        .collect::<Vec<String>>()
        .join("")
}

fn to_transaction_logs_entry(
    row: &Row,
    instance_name: &InstanceName,
    database_name: &str,
    sep: char,
) -> String {
    let name = row.get_value_by_name("name");
    let physical_name = row.get_value_by_name("physical_name");
    let max_size = row.get_bigint_by_name("MaxSize");
    let allocated_size = row.get_bigint_by_name("AllocatedSize");
    let used_size = row.get_bigint_by_name("UsedSize");
    let unlimited = row.get_value_by_name("Unlimited");
    format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}\n",
        instance_name,
        database_name.replace(' ', "_"),
        name.replace(' ', "_"),
        physical_name.replace(' ', "_"),
        max_size,
        allocated_size,
        used_size,
        unlimited
    )
}

fn to_datafiles_entries(
    instance_name: &InstanceName,
    database_name: &str,
    rows: &[Vec<Row>],
    sep: char,
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    rows[0]
        .iter()
        .map(|row| to_datafiles_entry(row, instance_name, database_name, sep))
        .collect::<Vec<String>>()
        .join("")
}

fn to_datafiles_entry(
    row: &Row,
    instance_name: &InstanceName,
    database_name: &str,
    sep: char,
) -> String {
    let name = row.get_value_by_name("name");
    let physical_name = row.get_value_by_name("physical_name");
    let max_size = row.get_bigint_by_name("MaxSize");
    let allocated_size = row.get_bigint_by_name("AllocatedSize");
    let used_size = row.get_bigint_by_name("UsedSize");
    let unlimited = row.get_value_by_name("Unlimited");
    format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}\n",
        instance_name,
        database_name.replace(' ', "_"),
        name.replace(' ', "_"),
        physical_name.replace(' ', "_"),
        max_size,
        allocated_size,
        used_size,
        unlimited
    )
}

fn to_databases_entries(instance_name: &InstanceName, rows: &[Vec<Row>], sep: char) -> String {
    if rows.is_empty() {
        return String::new();
    }
    rows[0]
        .iter()
        .map(|row| to_databases_entry(row, instance_name, sep))
        .collect::<Vec<String>>()
        .join("")
}

fn to_databases_entry(row: &Row, instance_name: &InstanceName, sep: char) -> String {
    let name = row.get_value_by_name("name");
    let status = row.get_value_by_name("Status");
    let recovery = row.get_value_by_name("Recovery");
    let auto_close = row.get_bigint_by_name("auto_close");
    let auto_shrink = row.get_bigint_by_name("auto_shrink");
    format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}\n",
        instance_name,
        name.replace(' ', "_").trim(),
        status.trim(),
        recovery.trim(),
        auto_close,
        auto_shrink,
    )
}

fn to_backup_entry(
    instance_name: &str,
    database_name: &str,
    row: &Row,
    sep: char,
) -> Option<String> {
    let last_backup_date = row.get_value_by_name("last_backup_date").trim().to_string();
    if last_backup_date.is_empty() {
        return None;
    }
    let backup_type = row.get_value_by_name("type").trim().to_string();
    let backup_type = if backup_type.is_empty() {
        "-".to_string()
    } else {
        backup_type
    };
    let replica_id = row.get_value_by_name("replica_id").trim().to_string();
    let is_primary_replica = row
        .get_value_by_name("is_primary_replica")
        .trim()
        .to_string();
    if replica_id.is_empty() && is_primary_replica == "True" {
        format!(
            "{}{sep}{}{sep}{}{sep}{}\n",
            instance_name,
            database_name.replace(' ', "_"),
            last_backup_date.replace(' ', "|"),
            backup_type,
        )
        .into()
    } else {
        None
    }
}

struct Counter {
    name: String,
    object: String,
    instance: String,
    value: String,
}

impl From<&Row> for Counter {
    fn from(row: &Row) -> Self {
        let instance = row.get_value_by_idx(2).trim().replace(' ', "_").to_string();
        Self {
            name: row
                .get_value_by_idx(0)
                .trim()
                .replace(' ', "_")
                .to_string()
                .to_lowercase(),
            object: row
                .get_value_by_idx(1)
                .trim()
                .replace([' ', '$'], "_")
                .to_string(),
            instance: if instance.is_empty() {
                "None".to_string()
            } else {
                instance
            },
            value: row.get_bigint_by_idx(3).to_string(),
        }
    }
}

impl Counter {
    pub fn into_string(self, sep: char) -> String {
        format!(
            "{}{sep}{}{sep}{}{sep}{}\n",
            self.object,
            self.name,
            if self.instance.is_empty() {
                "None"
            } else {
                &self.instance
            },
            self.value
        )
    }
}

fn to_counter_entry(row: &Row, sep: char) -> String {
    let counter = Counter::from(row);
    counter.into_string(sep)
}

fn to_blocked_session_entry(instance_name: &InstanceName, row: &Row, sep: char) -> String {
    let session_id = row.get_value_by_idx(0).trim().to_string();
    let wait_duration_ms = row.get_bigint_by_idx(1).to_string();
    let wait_type = row.get_value_by_idx(2).trim().to_string();
    let blocking_session_id = row.get_value_by_idx(3).trim().to_string();
    format!("{instance_name}{sep}{session_id}{sep}{wait_duration_ms}{sep}{wait_type}{sep}{blocking_session_id}\n",)
}

impl CheckConfig {
    pub async fn exec(&self, environment: &Env) -> Result<String> {
        if let Some(ms_sql) = self.ms_sql() {
            CheckConfig::prepare_cache_sub_dir(environment, ms_sql.hash());
            log::info!("Generating main data");
            let mut output: Vec<String> = Vec::new();
            output.push(
                generate_data(ms_sql, environment)
                    .await
                    .unwrap_or_else(|e| {
                        log::error!("Error generating data at main config: {e}");
                        format!("{e}\n")
                    }),
            );
            for (num, config) in std::iter::zip(0.., ms_sql.configs()) {
                log::info!("Generating configs data");
                CheckConfig::prepare_cache_sub_dir(environment, &config.cache_dir());
                let configs_data = generate_data(config, environment)
                    .await
                    .unwrap_or_else(|e| {
                        log::error!("Error generating data at config {num}: {e}");
                        format!("{e}\n")
                    });
                output.push(configs_data);
            }
            Ok(output.join(""))
        } else {
            log::error!("No config");
            anyhow::bail!("No Config")
        }
    }

    fn prepare_cache_sub_dir(environment: &Env, hash: &str) {
        match environment.obtain_cache_sub_dir(hash).map(utils::touch_dir) {
            Some(Err(e)) => log::error!("Error touching dir: {e}, caching may be not possible"),
            Some(Ok(p)) => log::info!("Using cache dir {p:?}"),
            None => log::warn!("No cache dir defined, caching is not possible"),
        }
    }
}

/// Generate header for each section without any data, see vbs plugin
fn generate_dumb_header(ms_sql: &config::ms_sql::Config) -> String {
    ms_sql
        .valid_sections()
        .iter()
        .map(|s| Section::new(s, Some(ms_sql.cache_age())).to_plain_header())
        .collect::<Vec<_>>()
        .join("")
}

fn generate_signaling_blocks(ms_sql: &config::ms_sql::Config, instances: &[SqlInstance]) -> String {
    instances
        .iter()
        .map(|i| i.piggyback().as_ref().cloned())
        .collect::<Vec<Option<PiggybackHostName>>>()
        .into_iter()
        .map(|h| generate_signaling_block(ms_sql, &h))
        .collect::<Vec<String>>()
        .join("")
}

fn generate_signaling_block(
    ms_sql: &config::ms_sql::Config,
    piggyback_host: &Option<PiggybackHostName>,
) -> String {
    let body = generate_dumb_header(ms_sql) + &Section::make_instance_section().to_plain_header();
    if let Some(piggyback_host) = piggyback_host.as_ref() {
        emit::piggyback_header(piggyback_host) + &body + &emit::piggyback_footer()
    } else {
        body
    }
}

/// Generate data as defined by config
/// Consists from two parts: instance entries + sections for every instance
async fn generate_data(ms_sql: &config::ms_sql::Config, environment: &Env) -> Result<String> {
    let instances = find_usable_instances(ms_sql, environment).await?;
    if instances.is_empty() {
        return Ok("ERROR: Failed to gather SQL server instances\n".to_string());
    } else {
        log::info!(
            "Found {} SQL server instances: [ {} ]",
            instances.len(),
            instances
                .iter()
                .map(|i| format!("{}", i))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let sections = ms_sql
        .valid_sections()
        .into_iter()
        .map(|s| {
            Section::new(
                s,
                if environment.disable_caching() {
                    None
                } else {
                    Some(ms_sql.cache_age())
                },
            )
        })
        .collect::<Vec<_>>();

    Ok(generate_signaling_blocks(ms_sql, &instances)
        + &generate_instance_entries(&instances)
        + &generate_result(&instances, &sections, ms_sql).await?)
}

fn generate_instance_entries<P: AsRef<SqlInstance>>(instances: &[P]) -> String {
    instances
        .iter()
        .map(|i| generate_instance_entry(i.as_ref()))
        .collect::<Vec<String>>()
        .join("")
}

fn generate_instance_entry<P: AsRef<SqlInstance>>(instance: &P) -> String {
    let section = Section::make_instance_section();
    [
        instance.as_ref().generate_header(),
        section.to_plain_header(),
        instance.as_ref().generate_leading_entry(section.sep()),
        instance.as_ref().generate_footer(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("")
}

async fn find_usable_instances(
    ms_sql: &config::ms_sql::Config,
    environment: &Env,
) -> Result<Vec<SqlInstance>> {
    let builders = find_usable_instance_builders(ms_sql).await?;
    if builders.is_empty() {
        log::warn!("Found NO usable SQL server instances");
        return Ok(Vec::new());
    } else {
        log::info!("Found {} usable SQL server instances", builders.len());
    }

    Ok(builders
        .into_iter()
        .map(|b: SqlInstanceBuilder| {
            b.environment(environment)
                .cache_dir(&ms_sql.cache_dir())
                .build()
        })
        .collect::<Vec<SqlInstance>>())
}

async fn find_usable_instance_builders(
    ms_sql: &config::ms_sql::Config,
) -> Result<Vec<SqlInstanceBuilder>> {
    Ok(find_all_instance_builders(ms_sql)
        .await?
        .into_iter()
        .filter(|i| ms_sql.is_instance_allowed(&i.get_name()))
        .collect::<Vec<SqlInstanceBuilder>>())
}

pub async fn find_all_instance_builders(
    ms_sql: &config::ms_sql::Config,
) -> Result<Vec<SqlInstanceBuilder>> {
    let found = find_detectable_instance_builders(ms_sql).await;

    let detected = if ms_sql.discovery().detect() {
        found
    } else {
        ms_sql
            .discovery()
            .include()
            .iter()
            .map(|name| SqlInstanceBuilder::new().name(name))
            .collect::<Vec<SqlInstanceBuilder>>()
    }
    .into_iter()
    .map(|b| b.piggyback(ms_sql.piggyback_host().map(|h| h.to_string().into())))
    .collect();
    // let d = HashSet::from_iter(detected) + HashSet::from_iter(predefined);
    let customizations: HashMap<&InstanceName, &CustomInstance> =
        ms_sql.instances().iter().map(|i| (i.name(), i)).collect();
    let builders = apply_customizations(detected, &customizations);
    add_custom_instance_builders(builders, &customizations).await
}

/// find instances described in the config but not detected by the discovery
async fn find_detectable_instance_builders(
    ms_sql: &config::ms_sql::Config,
) -> Vec<SqlInstanceBuilder> {
    obtain_instance_builders(&ms_sql.endpoint(), &[])
        .await
        .unwrap_or_else(|e| {
            log::warn!("Error discovering instances: {e}");
            vec![]
        })
}

/// find instances described in the config but not detected by the discovery
/// may NOT work - should be approved during testing
async fn add_custom_instance_builders(
    builders: Vec<SqlInstanceBuilder>,
    customizations: &HashMap<&InstanceName, &CustomInstance>,
) -> Result<Vec<SqlInstanceBuilder>> {
    let reconnects = determine_reconnect(builders, customizations);

    let mut builders: Vec<SqlInstanceBuilder> = Vec::new();
    for (builder, endpoint) in reconnects.into_iter() {
        if let Some(endpoint) = endpoint {
            if let Some(b) = get_custom_instance_builder(&builder, &endpoint).await {
                builders.push(b);
            }
        } else {
            builders.push(builder);
        }
    }
    Ok(builders)
}

async fn get_custom_instance_builder(
    builder: &SqlInstanceBuilder,
    endpoint: &Endpoint,
) -> Option<SqlInstanceBuilder> {
    let port = get_reasonable_port(builder, endpoint);
    let instance_name = &builder.get_name();
    log::debug!("Trying to connect to `{instance_name}` using config port {port}");
    let result = match client::connect_custom_endpoint(endpoint, port.clone()).await {
        Ok(mut client) => {
            let b = obtain_properties(&mut client, instance_name)
                .await
                .map(|p| to_instance_builder(endpoint, &p));
            if b.is_none() {
                log::info!("Instance `{instance_name}` not found. Try to find it");
                find_custom_instance(endpoint, instance_name).await
            } else {
                b
            }
        }
        Err(e) => {
            log::error!("Error creating client for `{instance_name}`: {e}");
            None
        }
    };
    #[cfg(unix)]
    return result;

    #[cfg(windows)]
    if result.is_none() {
        log::info!(
            "Instance `{instance_name}` at port {} not found. Try to use named connection.",
            port.clone()
        );
        match client::connect_custom_instance(endpoint, instance_name).await {
            Ok(mut client) => {
                let b = obtain_properties(&mut client, instance_name)
                    .await
                    .map(|p| to_instance_builder(endpoint, &p));
                if b.is_none() {
                    log::error!("Instance `{instance_name}` not found. Impossible.");
                }
                b
            }
            Err(e) => {
                log::warn!("Error creating client for `{instance_name}`: {e}");
                find_custom_instance(endpoint, instance_name).await
            }
        }
    } else {
        result
    }
}

async fn find_custom_instance(
    endpoint: &Endpoint,
    instance_name: &InstanceName,
) -> Option<SqlInstanceBuilder> {
    let builders = obtain_instance_builders(endpoint, &[instance_name])
        .await
        .unwrap_or_else(|e| {
            log::error!("Error creating client for instance `{instance_name}`: {e}",);
            Vec::<SqlInstanceBuilder>::new()
        });
    match detect_instance_port(instance_name, &builders) {
        Some(port) => {
            log::info!("Instance `{instance_name}` found at port {port}");
            if let Ok(mut client) = client::connect_custom_endpoint(endpoint, port.clone()).await {
                obtain_properties(&mut client, instance_name)
                    .await
                    .map(|p| to_instance_builder(endpoint, &p).port(Some(port)))
            } else {
                None
            }
        }
        _ => {
            log::error!(
                "Impossible to detect port for `{instance_name}` known: `{}`",
                builders
                    .iter()
                    .map(|i| i.get_name().into())
                    .collect::<Vec<String>>()
                    .join(", ")
            );
            None
        }
    }
}

fn detect_instance_port(name: &InstanceName, builders: &[SqlInstanceBuilder]) -> Option<Port> {
    builders
        .iter()
        .find(|b| b.get_name() == *name)
        .map(|b| b.get_port())
}

fn get_reasonable_port(builder: &SqlInstanceBuilder, endpoint: &Endpoint) -> Port {
    if builder.get_port() == Port(0) {
        log::info!("Connecting using port from endpoint {}", endpoint.port());
        endpoint.port()
    } else {
        log::info!(
            "Connecting using port from detection {}",
            builder.get_port()
        );
        builder.get_port()
    }
}

async fn obtain_properties(
    client: &mut Client,
    name: &InstanceName,
) -> Option<SqlInstanceProperties> {
    match SqlInstanceProperties::obtain_by_query(client).await {
        Ok(properties) => {
            if properties.name == *name {
                log::info!("Custom instance `{name}` added in query");
                return Some(properties);
            }
            log::error!(
                "Wrong instance: expected `{name}` but found `{}`",
                properties.name
            );
        }
        Err(e) => {
            log::error!("Error accessing instance `{name}` with error: {e:?}");
        }
    }
    None
}

/// converts detected instance and custom instance to SqlInstanceBuilder
fn to_instance_builder(
    endpoint: &Endpoint,
    properties: &SqlInstanceProperties,
) -> SqlInstanceBuilder {
    SqlInstanceBuilder::new()
        .name(properties.name.clone())
        .computer_name(Some(properties.computer_name.clone()))
        .version(&properties.version)
        .edition(&properties.edition)
        .port(Some(endpoint.conn().port()))
}
/// returns
/// - SQL instances with custom endpoint if any
fn determine_reconnect(
    builders: Vec<SqlInstanceBuilder>,
    customizations: &HashMap<&InstanceName, &CustomInstance>,
) -> Vec<(SqlInstanceBuilder, Option<Endpoint>)> {
    let mut found: HashSet<InstanceName> = HashSet::new();
    let mut b = builders
        .into_iter()
        .map(|instance_builder| {
            found.insert(instance_builder.get_name());
            match customizations.get(&instance_builder.get_name()) {
                Some(customization)
                    if Some(&customization.endpoint()) != instance_builder.get_endpoint() =>
                {
                    log::info!("Instance {} to be reconnected", instance_builder.get_name(),);
                    (instance_builder, Some(customization.endpoint()))
                }
                _ => {
                    log::info!(
                        "Add detected instance {} reconnect not required ",
                        &instance_builder.get_name()
                    );
                    (instance_builder, None)
                }
            }
        })
        .collect::<Vec<(SqlInstanceBuilder, Option<Endpoint>)>>();

    customizations
        .iter()
        .filter(|(&k, _)| !found.contains(k))
        .map(|(&name, customization)| {
            log::info!("Add custom instance {} ", name);
            let builder = SqlInstanceBuilder::new().name(name.clone());
            (
                apply_customization(builder, customization),
                Some(customization.endpoint()),
            )
        })
        .for_each(|a| b.push(a));

    b
}

fn apply_customizations(
    detected: Vec<SqlInstanceBuilder>,
    customizations: &HashMap<&InstanceName, &CustomInstance>,
) -> Vec<SqlInstanceBuilder> {
    detected
        .into_iter()
        .map(
            |instance_builder| match customizations.get(&instance_builder.get_name()) {
                Some(customization) => apply_customization(instance_builder, customization),
                None => instance_builder.clone(),
            },
        )
        .collect::<Vec<SqlInstanceBuilder>>()
}

fn apply_customization(
    builder: SqlInstanceBuilder,
    customization: &CustomInstance,
) -> SqlInstanceBuilder {
    builder
        .piggyback(
            customization
                .piggyback()
                .map(|p| p.hostname())
                .map(|h| h.clone().into()),
        )
        .alias(customization.alias())
}

/// Intelligent async processing of the data
async fn generate_result(
    instances: &[SqlInstance],
    sections: &[Section],
    ms_sql: &config::ms_sql::Config,
) -> Result<String> {
    // place all futures now in vector for future asynchronous processing
    let tasks = instances
        .iter()
        .map(move |instance| instance.generate_sections(ms_sql, sections));

    // processing here
    let s: u32 = ms_sql.options().max_connections().into();
    let results = stream::iter(tasks)
        .buffer_unordered(s as usize)
        .collect::<Vec<_>>()
        .await;

    Ok(results.join(""))
}

// TODO(sk):probably normal SQL query  is better than registry reading SQL query
/// obtain all instances from endpoint, on Windows can try SQL Browser
pub async fn obtain_instance_builders(
    endpoint: &Endpoint,
    instances: &[&InstanceName],
) -> Result<Vec<SqlInstanceBuilder>> {
    match client::connect_main_endpoint(endpoint).await {
        Ok(mut client) => Ok(_obtain_instance_builders(&mut client, endpoint).await),
        Err(err) => {
            log::error!("Failed to create main client: {err}");
            obtain_instance_builders_by_sql_browser(endpoint, instances).await
        }
    }
}

#[cfg(windows)]
pub async fn obtain_instance_builders_by_sql_browser(
    endpoint: &Endpoint,
    instances: &[&InstanceName],
) -> Result<Vec<SqlInstanceBuilder>> {
    log::info!("Finding instances by SQL Browser");
    for instance in instances {
        match client::ClientBuilder::new()
            .browse(
                endpoint.conn().hostname(),
                instance,
                endpoint.conn().sql_browser_port(),
            )
            .build()
            .await
        {
            Ok(mut client) => return Ok(_obtain_instance_builders(&mut client, endpoint).await),
            Err(err) => {
                log::error!("Failed to create client: {err}");
            }
        }
    }
    anyhow::bail!("Impossible to connect")
}

#[cfg(unix)]
pub async fn obtain_instance_builders_by_sql_browser(
    _endpoint: &Endpoint,
    _instances: &[&InstanceName],
) -> Result<Vec<SqlInstanceBuilder>> {
    anyhow::bail!("Failed to create client, sql browser on linux is not supported")
}

async fn _obtain_instance_builders(
    client: &mut Client,
    endpoint: &Endpoint,
) -> Vec<SqlInstanceBuilder> {
    let mut builders = try_find_instances_in_registry(client).await;
    if builders.is_empty() {
        log::warn!("No instances found in registry, this means you have problem with permissions");
        log::warn!("Trying to add current instance");
        match obtain_instance_name(client).await {
            Ok(Some(name)) => {
                let mut builder = SqlInstanceBuilder::new()
                    .name(name)
                    .port(Some(endpoint.conn().port()));
                if let Ok(properties) = SqlInstanceProperties::obtain_by_query(client).await {
                    builder = builder
                        .version(&properties.version)
                        .edition(&properties.edition);
                }
                builders = vec![builder];
            }
            _ => {
                log::error!("Can't add current instance");
                return vec![];
            }
        };
    }
    let computer_name = obtain_computer_name(client).await.unwrap_or_default();
    builders
        .iter()
        .map(|i| {
            i.clone()
                .endpoint(endpoint)
                .computer_name(computer_name.clone())
        })
        .collect()
}

/// returns instances found in registry
/// if registry is unavailable returns empty list, this is ok too
async fn try_find_instances_in_registry(client: &mut Client) -> Vec<SqlInstanceBuilder> {
    let mut result: Vec<SqlInstanceBuilder> = vec![];
    for q in [
        &sqls::get_win_registry_instances_query(),
        &sqls::get_wow64_32_registry_instances_query(),
    ] {
        let instances = exec_win_registry_sql_instances_query(client, q)
            .await
            .unwrap_or_else(|e| {
                log::info!("Can't get normal instances: {e}, it is not error");
                Vec::new()
            });
        result.extend(instances);
    }
    log::debug!("Found in registry {:#?}", result);
    result
}

/// return all MS SQL instances installed
async fn exec_win_registry_sql_instances_query(
    client: &mut Client,
    query: &str,
) -> Result<Vec<SqlInstanceBuilder>> {
    let answers = run_custom_query(client, query).await?;
    if let Some(rows) = answers.first() {
        let instances = to_sql_instance(rows);
        log::info!(
            "Instances found in registry by SQL query on main instance {}",
            instances.len()
        );
        Ok(instances)
    } else {
        log::warn!("Empty answer by query: {query}");
        Ok(vec![])
    }
}

fn to_sql_instance(rows: &Answer) -> Vec<SqlInstanceBuilder> {
    rows.iter()
        .map(|r| SqlInstanceBuilder::new().row(r))
        .collect::<Vec<SqlInstanceBuilder>>()
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::{
        generate_instance_entries, generate_signaling_blocks, SqlInstance, SqlInstanceBuilder,
    };
    use crate::args::Args;
    use crate::setup::Env;
    use crate::types::Port;
    use std::path::Path;

    #[test]
    fn test_generate_state_entry() {
        let i = SqlInstanceBuilder::new().name("test_name").build();

        assert_eq!(
            i.generate_bad_state_entry('.', "bad"),
            format!("MSSQL_TEST_NAME.state.0.bad\n")
        );
        assert_eq!(
            i.generate_good_state_entry('.'),
            format!("MSSQL_TEST_NAME.state.1.\n")
        );
    }

    fn make_instances() -> Vec<SqlInstance> {
        let builders = vec![
            SqlInstanceBuilder::new().name("A"),
            SqlInstanceBuilder::new()
                .name("B")
                .piggyback(Some("Y".to_string().into())),
        ];

        builders
            .into_iter()
            .map(|b: SqlInstanceBuilder| b.build())
            .collect::<Vec<SqlInstance>>()
    }
    #[test]
    fn test_instance_entries() {
        let instances = make_instances();
        assert_eq!(
            generate_instance_entries(&instances),
            "\
             <<<mssql_instance:sep(124)>>>\n\
             MSSQL_A|config|||\n\
             <<<<y>>>>\n\
             <<<mssql_instance:sep(124)>>>\n\
             MSSQL_B|config|||\n\
             <<<<>>>>\n\
             "
        );
    }

    #[test]
    fn test_signaling_blocks() {
        const CONFIG_WITH_INSTANCES: &str = r#"---
mssql:
  main: # mandatory, to be used if no specific config
    authentication: # mandatory
      username: u # mandatory
      password: u
      type: sql_server
    connection:
      hostname: am
    sections:
      - instance:
      - backup:
          is_async: yes
      - counters:
          disabled: yes
    discovery:
      detect: no
      include: [ "A", "B"]
    instances:
      - sid: "B"
        piggyback:
          hostname: Y
"#;
        // blocks below may follow in any order
        const EXPECTED_OK_BLOCK: &str = "\
        <<<mssql_instance:sep(124)>>>\n\
        <<<mssql_backup:sep(124)>>>\n\
        <<<mssql_instance:sep(124)>>>\n\
        ";
        const EXPECTED_PB_BLOCK: &str = "\
        <<<<y>>>>\n\
        <<<mssql_instance:sep(124)>>>\n\
        <<<mssql_backup:sep(124)>>>\n\
        <<<mssql_instance:sep(124)>>>\n\
        <<<<>>>>\n\
        ";
        let ms_sql = crate::config::ms_sql::Config::from_string(CONFIG_WITH_INSTANCES)
            .unwrap()
            .unwrap();
        let instances = make_instances();
        let blocks = generate_signaling_blocks(&ms_sql, &instances);
        assert!(blocks.contains(EXPECTED_OK_BLOCK));
        assert!(blocks.contains(EXPECTED_PB_BLOCK));
        assert_eq!(
            blocks.len(),
            EXPECTED_OK_BLOCK.len() + EXPECTED_PB_BLOCK.len(),
            "{}",
            blocks
        );
    }

    #[test]
    fn test_instance_header_footer() {
        let normal = SqlInstanceBuilder::new().name("test_name").build();
        assert_eq!(normal.generate_header(), "");
        assert_eq!(normal.generate_footer(), "");
        let piggyback = SqlInstanceBuilder::new()
            .name("test_name")
            .piggyback(Some("Y".to_string().into()))
            .build();
        assert_eq!(piggyback.generate_header(), "<<<<y>>>>\n");
        assert_eq!(piggyback.generate_footer(), "<<<<>>>>\n");
    }

    #[test]
    fn test_sql_builder() {
        let args = Args {
            temp_dir: Some(".".into()),
            ..Default::default()
        };
        let standard = SqlInstanceBuilder::new()
            .name("name")
            .alias(&Some("alias".to_string().into()))
            .dynamic_port(Some(Port(1u16)))
            .port(Some(Port(2u16)))
            .computer_name(Some("computer_name".to_string().into()))
            .cache_dir("hash")
            .version(&"version".to_string().into())
            .edition(&"edition".to_string().into())
            .environment(&Env::new(&args))
            .id("id")
            .piggyback(Some("piggYback".to_string().into()));
        assert_eq!(standard.get_port(), Port(2u16));
        let cluster = standard.clone().cluster(Some("cluster".to_string().into()));

        let s = standard.build();
        assert_eq!(s.id.to_string(), "id");
        assert_eq!(s.name.to_string(), "NAME");
        assert_eq!(s.alias, Some("alias".to_string().into()));
        assert!(s.cluster.is_none());
        assert_eq!(s.version.to_string(), "version");
        assert_eq!(s.edition.to_string(), "edition");
        assert_eq!(s.cache_dir, "hash");
        assert_eq!(s.port, Some(Port(2u16)));
        assert_eq!(s.dynamic_port, Some(Port(1u16)));

        assert_eq!(s.piggyback(), &Some("piggyback".to_string().into()));
        assert_eq!(s.computer_name(), &Some("computer_name".to_string().into()));
        assert_eq!(s.temp_dir(), Some(Path::new(".")));
        assert_eq!(s.full_name(), "localhost/NAME");
        assert_eq!(s.mssql_name(), "MSSQL_NAME");
        assert_eq!(s.legacy_name(), "computer_name/NAME");
        assert_eq!(
            s.generate_leading_entry('.'),
            "MSSQL_NAME.config.version.edition.\n"
        );

        let c = cluster.build();
        assert_eq!(c.cluster, Some("cluster".to_string().into()));
        assert_eq!(c.legacy_name(), "cluster/NAME");
        assert_eq!(
            c.generate_leading_entry('.'),
            "MSSQL_NAME.config.version.edition.cluster\n"
        );
    }
}
