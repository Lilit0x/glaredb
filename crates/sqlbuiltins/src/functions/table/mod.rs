//! Builtin table returning functions.
mod bigquery;
mod bson;
mod cassandra;
mod clickhouse;
mod delta;
mod excel;
mod generate_series;
mod iceberg;
mod lance;
mod mongodb;
mod mysql;
mod object_store;
mod postgres;
mod snowflake;
mod sqlserver;
mod system;
mod virtual_listing;

use ::object_store::aws::AmazonS3ConfigKey;
use ::object_store::azure::AzureConfigKey;
use ::object_store::gcp::GoogleConfigKey;
use async_trait::async_trait;
use datafusion::datasource::TableProvider;
use datafusion_ext::errors::{ExtensionError, Result};
use datafusion_ext::functions::{FuncParamValue, IdentValue, TableFuncContextProvider};
use datasources::common::url::{DatasourceUrl, DatasourceUrlType};
use protogen::metastore::types::catalog::RuntimePreference;
use protogen::metastore::types::options::{CredentialsOptions, StorageOptions};
use std::collections::HashMap;
use std::sync::Arc;

use self::bigquery::ReadBigQuery;
use self::bson::BsonScan;
use self::cassandra::ReadCassandra;
use self::clickhouse::ReadClickhouse;
use self::delta::DeltaScan;
use self::excel::ExcelScan;
use self::generate_series::GenerateSeries;
use self::iceberg::{data_files::IcebergDataFiles, scan::IcebergScan, snapshots::IcebergSnapshots};
use self::lance::LanceScan;
use self::mongodb::ReadMongoDb;
use self::mysql::ReadMysql;
use self::object_store::{READ_CSV, READ_JSON, READ_PARQUET};
use self::postgres::ReadPostgres;
use self::snowflake::ReadSnowflake;
use self::sqlserver::ReadSqlServer;
use self::system::cache_external_tables::CacheExternalDatabaseTables;
use self::virtual_listing::{ListColumns, ListSchemas, ListTables};

use super::BuiltinFunction;

/// A builtin table function.
/// Table functions are ones that are used in the FROM clause.
/// e.g. `SELECT * FROM my_table_func(...)`
#[async_trait]
pub trait TableFunc: BuiltinFunction {
    /// Determine the runtime preference for the function from the passed-on
    /// arguments.
    fn detect_runtime(
        &self,
        _args: &[FuncParamValue],
        _parent: RuntimePreference,
    ) -> Result<RuntimePreference>;

    /// Return a table provider using the provided args.
    async fn create_provider(
        &self,
        ctx: &dyn TableFuncContextProvider,
        args: Vec<FuncParamValue>,
        opts: HashMap<String, FuncParamValue>,
    ) -> Result<Arc<dyn TableProvider>>;
}

/// All builtin table functions.
pub struct BuiltinTableFuncs {
    pub funcs: HashMap<String, Arc<dyn TableFunc>>,
}

impl BuiltinTableFuncs {
    pub fn new() -> BuiltinTableFuncs {
        let funcs: Vec<Arc<dyn TableFunc>> = vec![
            // Databases/warehouses
            Arc::new(ReadPostgres),
            Arc::new(ReadBigQuery),
            Arc::new(ReadMongoDb),
            Arc::new(ReadMysql),
            Arc::new(ReadSnowflake),
            Arc::new(ReadClickhouse),
            Arc::new(ReadSqlServer),
            Arc::new(ReadCassandra),
            // Object store
            Arc::new(READ_PARQUET),
            Arc::new(READ_CSV),
            Arc::new(READ_JSON),
            Arc::new(BsonScan),
            // Data lakes
            Arc::new(DeltaScan),
            Arc::new(IcebergScan),
            Arc::new(IcebergSnapshots),
            Arc::new(IcebergDataFiles),
            Arc::new(ExcelScan),
            Arc::new(LanceScan),
            // Listing
            Arc::new(ListSchemas),
            Arc::new(ListTables),
            Arc::new(ListColumns),
            // Series generating
            Arc::new(GenerateSeries),
            // System operations
            Arc::new(CacheExternalDatabaseTables),
        ];

        let funcs: HashMap<String, Arc<dyn TableFunc>> = funcs
            .into_iter()
            .flat_map(|func| {
                // Ensure function can be referenced through name or alias.
                [func.aliases(), &[func.name()]]
                    .concat()
                    .into_iter()
                    .map(|name| (name.to_string(), func.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();

        BuiltinTableFuncs { funcs }
    }

    pub fn find_function(&self, name: &str) -> Option<&Arc<dyn TableFunc>> {
        self.funcs.get(name)
    }

    pub fn iter_funcs(&self) -> impl Iterator<Item = &Arc<dyn TableFunc>> {
        self.funcs.values()
    }
}

impl Default for BuiltinTableFuncs {
    fn default() -> Self {
        Self::new()
    }
}

// Parse the data lake table location and object store options from the provided function arguments
pub fn table_location_and_opts(
    ctx: &dyn TableFuncContextProvider,
    args: Vec<FuncParamValue>,
    opts: &mut HashMap<String, FuncParamValue>,
) -> Result<(DatasourceUrl, StorageOptions)> {
    let mut args = args.into_iter();
    let first = args
        .next()
        .ok_or_else(|| ExtensionError::ExpectedIndexedArgument {
            index: 0,
            what: "location for the table".to_string(),
        })?;

    let url: String = first.try_into()?;
    let source_url =
        DatasourceUrl::try_new(url).map_err(|e| ExtensionError::Access(Box::new(e)))?;

    let mut maybe_cred_opts = None;
    // Check if a credentials object has been supplied
    if let Some(func_param) = args.next() {
        let creds: IdentValue = func_param.try_into()?;
        maybe_cred_opts = Some(
            ctx.get_session_catalog()
                .resolve_credentials(creds.as_str())
                .cloned()
                .ok_or(ExtensionError::String(format!(
                    "missing credentials object: {creds}"
                )))?
                .options,
        );
    }

    let mut storage_options = StorageOptions::default();
    match (source_url.datasource_url_type(), maybe_cred_opts) {
        (DatasourceUrlType::File, None) => {} // no options fine in this case
        (DatasourceUrlType::File, _) => {
            return Err(ExtensionError::String(
                "Credentials incorrectly provided when accessing local delta table".to_string(),
            ))
        }
        (DatasourceUrlType::Gcs, Some(CredentialsOptions::Gcp(creds))) => {
            storage_options.inner.insert(
                GoogleConfigKey::ServiceAccountKey.as_ref().to_string(),
                creds.service_account_key,
            );
        }
        (DatasourceUrlType::S3, Some(CredentialsOptions::Aws(creds))) => {
            const REGION_KEY: &str = "region";
            let region = opts
                .remove(REGION_KEY)
                .ok_or(ExtensionError::MissingNamedArgument(REGION_KEY))?
                .try_into()?;

            storage_options.inner.insert(
                AmazonS3ConfigKey::AccessKeyId.as_ref().to_string(),
                creds.access_key_id,
            );
            storage_options.inner.insert(
                AmazonS3ConfigKey::SecretAccessKey.as_ref().to_string(),
                creds.secret_access_key,
            );
            storage_options
                .inner
                .insert(AmazonS3ConfigKey::Region.as_ref().to_string(), region);
        }
        (DatasourceUrlType::Azure, Some(CredentialsOptions::Azure(creds))) => {
            storage_options.inner.insert(
                AzureConfigKey::AccountName.as_ref().to_string(),
                creds.account_name,
            );
            storage_options.inner.insert(
                AzureConfigKey::AccessKey.as_ref().to_string(),
                creds.access_key,
            );
        }
        (DatasourceUrlType::Http, _) => {
            return Err(ExtensionError::String(
                "Accessing delta tables over http not supported".to_string(),
            ))
        }
        (datasource, creds) => {
            return Err(ExtensionError::String(format!(
                "Invalid credentials for {datasource}, got {} creds",
                if let Some(o) = creds {
                    o.as_str()
                } else {
                    "no"
                }
            )))
        }
    };

    Ok((source_url, storage_options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_scan_function_aliases() {
        // Ensure we can get functions using both their names and aliases (e.g.
        // 'parquet_scan' aliased to 'read_parquet').

        let names_and_aliases = [
            "read_csv", // name
            "csv_scan", // alias
            "read_ndjson",
            "ndjson_scan",
            "read_parquet",
            "parquet_scan",
        ];

        let funcs = BuiltinTableFuncs::new();

        for name in names_and_aliases {
            funcs
                .find_function(name)
                .expect(&format!("function with name '{name}' should exist"));
        }
    }
}
